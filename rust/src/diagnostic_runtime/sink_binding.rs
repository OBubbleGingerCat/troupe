use std::sync::Arc;

use pyo3::prelude::*;
use troupe_agent_runtime::AgentTurnControl;

use crate::orchestration::scene_context::{CuedScope, RunBinding};

#[cfg(test)]
use super::act_producer;
use super::hooks::DiagnosticActBinding;

pub(crate) fn admit_act(
    py: Python<'_>,
    run: &RunBinding,
    cued: &Arc<CuedScope>,
    control: &Arc<AgentTurnControl>,
    binding: DiagnosticActBinding,
) -> PyResult<()> {
    #[cfg(not(test))]
    return active::admit_act(py, run, cued, control, binding);

    #[cfg(test)]
    {
        let _ = (py, binding);
        act_producer::admitted(run, cued, control);
        Ok(())
    }
}

#[cfg(not(test))]
#[allow(unused_imports)]
pub(crate) use active::{
    ActSinkAdmissionCapability, BoundActSink, bound_sink_for, production_capability,
};

#[cfg(not(test))]
mod active {
    use std::any::Any;
    use std::collections::HashMap;
    use std::fmt;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex, MutexGuard, OnceLock, Weak};
    use std::time::{Duration, Instant};

    use pyo3::class::gc::{PyTraverseError, PyVisit};
    use pyo3::exceptions::{PyAttributeError, PyRuntimeError};
    use pyo3::prelude::*;
    use pyo3::types::{PyAny, PyAnyMethods, PyDict, PyModule, PyTuple, PyType};
    use troupe_agent_runtime::diagnostics::payload::{
        AgentToolPayloadActBudget, SinkOnlyToolPayload, ToolPayloadSource,
    };
    use troupe_agent_runtime::{
        AgentDiagnosticErrorCode, AgentDiagnosticFailureOwner, AgentDiagnosticObserver,
        AgentDiagnosticObserverFailure, AgentSessionDiagnosticContext, AgentTurnControl,
        ToolPayloadCapturePolicy,
    };
    use troupe_diagnostics_core::detail::{DiagnosticComponentFailedDetail, InstantDetail};
    use troupe_diagnostics_core::event::{CausalLink, DiagnosticEvent, DiagnosticScope};
    use troupe_diagnostics_core::hub::{
        AcceptedDiagnosticEvent, ActEventSubscriber, AdmissionReservation, AdmissionReserver,
        AdmissionSize, BoundedInMemoryReserver, DeliveryFailure, SinkOnlyDiagnosticHub,
    };
    use troupe_diagnostics_core::id::{CanonicalUuid, RunLocalId};
    use troupe_diagnostics_core::kinds::{
        CausalRelation, ComponentFailureErrorCode, ComponentFailureStage, CounterKind, InstantKind,
        SpanKind, SpanOutcome,
    };
    use troupe_diagnostics_core::scalar::SchemaU64;
    use troupe_diagnostics_core::time::RunClock;
    use uuid::Uuid;

    use crate::diagnostic_runtime::act_producer::{
        ActDiagnosticFailureOwner, PreparedActAdmission, StandaloneActAdmission,
    };
    use crate::diagnostic_runtime::hooks::{
        DiagnosticActBinding, DiagnosticActSubscriberLookup, DiagnosticAdmissionCapability,
        DiagnosticAdmissionProfile, DiagnosticCaptureConfig,
    };
    use crate::diagnostic_runtime::load_producer::{
        DiagnosticActSubscriberInstallError, DiagnosticProducerError, DiagnosticRunContext,
    };
    use crate::diagnostic_runtime::sink_projection::{
        PreparedSinkToolPayload, SinkProjectedEvent, SinkProjectedJsonValue,
        prepare_sink_tool_payload, project_act_event,
    };
    use crate::diagnostic_runtime::sink_settlement::{
        ActAuthorityExpiry, ActAuthorityExpiryInstallError, ActSettlementCoordinator,
        ActSettlementError, ActSettlementSink, ActSettlementSinkCommit, ActSinkSettlement,
    };
    use crate::diagnostic_runtime::usage_finalization::{
        UsageFinalizationFailureOwner, UsageFinalizingObservationBridge,
        UsageObservationDisposition,
    };
    use crate::diagnostic_runtime::{act_producer, custom_act_binding, runtime_producer};
    use crate::diagnostic_sink::{
        ActOutcome, AdmissionClass, AdmissionOutcome, DiagnosticSinkRuntime, DispatchEvent,
        SinkDeliveryFailure, SinkHandle,
    };
    use crate::orchestration::scene_context::{CuedScope, RunBinding};

    const STANDALONE_EVENT_MAX_BYTES: usize = 8 * 1024 * 1024;
    const BINDING_SHUTDOWN_GRACE: Duration = Duration::from_millis(100);
    const SINK_DELIVERY_FAILED: &str = "sink_delivery_failed";
    const SINK_SETTLEMENT_FAILED: &str = "sink_settlement_failed";

    fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
        mutex
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    pub(super) fn admit_act(
        py: Python<'_>,
        run: &RunBinding,
        cued: &Arc<CuedScope>,
        control: &Arc<AgentTurnControl>,
        binding: DiagnosticActBinding,
    ) -> PyResult<()> {
        if let Some(capability) = run.diagnostic_admission().capability() {
            return capability.admit_act(py, run, cued, control, binding);
        }
        if !binding.is_active() {
            act_producer::admitted(run, cued, control);
            return Ok(());
        }
        if runtime_producer::producer_for_binding(run).is_some() {
            return Err(PyRuntimeError::new_err(
                "Production diagnostics did not install its mandatory Act sink capability",
            ));
        }

        let candidate = ActSinkAdmissionCapability::standalone().map_err(|error| {
            PyRuntimeError::new_err(format!("initialize standalone diagnostics: {error}"))
        })?;
        let erased: Arc<dyn DiagnosticAdmissionCapability> = candidate.clone();
        let capability = match run.diagnostic_admission().install(erased) {
            Ok(()) => candidate,
            Err(_) => run
                .diagnostic_admission()
                .capability()
                .expect("a failed capability install has a winner")
                .clone(),
        };
        capability.admit_act(py, run, cued, control, binding)
    }

    pub(crate) fn production_capability(
        run_id: CanonicalUuid,
        context: DiagnosticRunContext,
        failure_owner: Arc<dyn ActDiagnosticFailureOwner>,
        usage: Arc<UsageFinalizingObservationBridge>,
    ) -> Result<Arc<ActSinkAdmissionCapability>, DiagnosticActSubscriberInstallError> {
        let registry = Arc::new(ActSinkRegistry::default());
        let lookup: Arc<dyn DiagnosticActSubscriberLookup> = registry.clone();
        context.install_act_subscriber_lookup(lookup)?;
        Ok(Arc::new(ActSinkAdmissionCapability {
            state: Arc::new(CapabilityState {
                profile: DiagnosticAdmissionProfile::ProductionDurable,
                run_id,
                context,
                registry,
                failure_owner,
                usage,
                standalone: None,
                sink_runtime: Mutex::new(None),
            }),
        }))
    }

    pub(crate) struct ActSinkAdmissionCapability {
        state: Arc<CapabilityState>,
    }

    impl ActSinkAdmissionCapability {
        fn standalone() -> Result<Arc<Self>, AgentDiagnosticErrorCode> {
            let run_id = CanonicalUuid::new(Uuid::new_v4());
            let clock = RunClock::from_origin(Instant::now());
            let registry = Arc::new(ActSinkRegistry::default());
            let lookup: Arc<dyn DiagnosticActSubscriberLookup> = registry.clone();
            let hub = Arc::new(SinkOnlyDiagnosticHub::sink_only(run_id, StandaloneReserver));
            let context =
                DiagnosticRunContext::sink_only(Arc::clone(&hub), clock, Arc::clone(&lookup));
            let local_owner = Arc::new(StandaloneFailureOwner::default());
            let usage_owner: Arc<dyn UsageFinalizationFailureOwner> = local_owner.clone();
            let usage = UsageFinalizingObservationBridge::sink_only_with_subscribers(
                hub,
                lookup,
                clock,
                usage_owner,
            )?;
            let observer = AgentDiagnosticObserver::new(usage.clone(), Arc::clone(&local_owner));
            let failure_owner: Arc<dyn ActDiagnosticFailureOwner> = local_owner;
            Ok(Arc::new(Self {
                state: Arc::new(CapabilityState {
                    profile: DiagnosticAdmissionProfile::SinkOnlyVolatile,
                    run_id,
                    context,
                    registry,
                    failure_owner,
                    usage: Arc::clone(&usage),
                    standalone: Some(StandaloneResources {
                        observer,
                        lineages: Mutex::new(StandaloneLineages::default()),
                    }),
                    sink_runtime: Mutex::new(None),
                }),
            }))
        }

        fn prepare(
            &self,
            run: &RunBinding,
            cued: &Arc<CuedScope>,
            control: &Arc<AgentTurnControl>,
        ) -> PyResult<PreparedActAdmission> {
            let standalone = match &self.state.standalone {
                None => None,
                Some(resources) => {
                    let lineage = lock(&resources.lineages)
                        .resolve(cued)
                        .map_err(admission_state_error)?;
                    let session = control
                        .snapshot_standalone_diagnostic_metadata(lineage.session)
                        .map_err(|error| {
                            PyRuntimeError::new_err(format!(
                                "snapshot standalone diagnostic session: {error}"
                            ))
                        })?;
                    Some(StandaloneActAdmission::new(
                        self.state.context.clone(),
                        lineage.cue_scope,
                        session,
                        Arc::clone(&self.state.failure_owner),
                    ))
                }
            };
            act_producer::prepare_admission(run, cued, control, standalone)
                .map_err(|error| PyRuntimeError::new_err(error.to_string()))?
                .ok_or_else(|| {
                    PyRuntimeError::new_err(
                        "diagnostic Act admission capability has no producer context",
                    )
                })
        }

        fn bind(
            &self,
            py: Python<'_>,
            run: &RunBinding,
            cued: &Arc<CuedScope>,
            control: &Arc<AgentTurnControl>,
            binding: DiagnosticActBinding,
        ) -> PyResult<()> {
            let (capture, request) = binding.into_parts();
            let request = request.expect("an active diagnostic binding retains its sink");
            let diagnostics = diagnostics_module(py)?;
            let sink_type = diagnostics
                .getattr("DiagnosticSink")?
                .cast_into::<PyType>()?;
            let sink = request.bind(py);
            let sink_lock =
                require_base_slot(&diagnostics, &sink_type, sink, "_DiagnosticSink__lock")?;
            sink_lock.call_method0("acquire")?;
            let _sink_lock = PythonLockGuard::new(sink_lock.unbind());
            let state =
                require_base_slot(&diagnostics, &sink_type, sink, "_DiagnosticSink__state")?
                    .extract::<String>()?;
            let bind_method = sink_type
                .getattr("__dict__")?
                .get_item("_diagnostic_bind")?;
            if state != "UNBOUND" {
                if !matches!(state.as_str(), "BOUND" | "SEALED" | "CLOSED") {
                    return Err(PyRuntimeError::new_err(
                        "diagnostic sink lifecycle state is corrupt",
                    ));
                }
                bind_method.call1((sink,))?;
                unreachable!("the exact base bind rejects a reused sink")
            }

            let prepared = self.prepare(run, cued, control)?;
            let act_id = prepared
                .act_scope()
                .act_id()
                .expect("a prepared Act has an Act ID")
                .clone();
            let event_context = prepared.context().without_act_subscriber_lookup();
            let mut prepared_authority =
                custom_act_binding::prepare(py, run, cued, prepared.act_scope())?;
            let reservation = self.state.registry.reserve(act_id.as_str())?;
            let callback = sink.getattr("on_event")?.unbind();
            let handle = self
                .state
                .register_sink(py, act_id.clone(), callback)
                .map_err(PyRuntimeError::new_err)?;
            let failure_facts = Arc::new(DeliveryFactEmitter::new(
                Arc::downgrade(&self.state),
                event_context.clone(),
                prepared.act_scope().clone(),
                handle.id(),
            ));
            let observed_failure_facts = Arc::clone(&failure_facts);
            handle
                .install_failure_observer(Arc::new(move |failure| {
                    observed_failure_facts.delivery_failed(failure);
                }))
                .map_err(|error| PyRuntimeError::new_err(error.to_string()))?;
            let settlement = ActSinkSettlement::new(
                handle.clone(),
                request.clone_ref(py),
                Arc::clone(&self.state.failure_owner),
            );
            let subscriber = Arc::new(ActSinkSubscriber {
                state: Arc::downgrade(&self.state),
                context: event_context,
                capture,
                act_scope: prepared.act_scope().clone(),
                authority: prepared_authority
                    .as_ref()
                    .map(custom_act_binding::PreparedActAuthority::authority),
                request: request.clone_ref(py),
                handle,
                payload: Mutex::new(PayloadState::default()),
                drop_facts: Mutex::new(DropFactState::default()),
                failure_facts,
                settlement,
                terminal_enqueued: AtomicBool::new(false),
            });
            if let Some(authority) = &prepared_authority {
                subscriber
                    .install_authority_expiry(authority.expiry())
                    .map_err(|error| PyRuntimeError::new_err(error.to_string()))?;
            }

            let standalone_observer = self
                .state
                .standalone
                .as_ref()
                .map(|resources| resources.observer.clone());
            let context = control.new_diagnostic_context(
                prepared.identity().clone(),
                standalone_observer,
                ToolPayloadCapturePolicy::new(capture.tool_inputs, capture.tool_outputs),
            );
            // Diagnostic context installation is the last resource acquisition that can fail. If
            // attachment rejects an invalid control, the sink is still UNBOUND, the reservation
            // drops unpublished, and the unreachable handle is closed by capability shutdown.
            control
                .install_diagnostic_context(context)
                .map_err(|error| {
                    self.state
                        .failure_owner
                        .latch_state_failure("act.sink-context-attach-failed");
                    PyRuntimeError::new_err(format!("attach diagnostic sink context: {error}"))
                })?;

            if let Some(authority) = prepared_authority.as_mut() {
                authority.stage(py)?;
            }
            // This is the exact base method under its exact RLock after an UNBOUND check, so no
            // user override or competing bind can introduce a fallible semantic transition here.
            bind_method.call1((sink,))?;
            reservation.publish(Arc::clone(&subscriber));
            // Act IDs come from a checked monotonic counter, and the prepared producer retains this
            // ActCall control in the prepared producer. Its defensive registry-race branch is
            // therefore unreachable on this valid path, so commit cannot follow a half-publish.
            prepared.commit();
            if let Some(authority) = prepared_authority {
                authority.commit();
            }
            self.bind_usage(&act_id)
        }

        fn admit_without_sink(
            &self,
            py: Python<'_>,
            run: &RunBinding,
            cued: &Arc<CuedScope>,
            control: &Arc<AgentTurnControl>,
        ) -> PyResult<()> {
            let Some(prepared) = act_producer::prepare_admission(run, cued, control, None)
                .map_err(|error| PyRuntimeError::new_err(error.to_string()))?
            else {
                return Ok(());
            };
            let act_id = prepared
                .act_scope()
                .act_id()
                .expect("a prepared Act has an Act ID")
                .clone();
            let Some(mut prepared_authority) =
                custom_act_binding::prepare(py, run, cued, prepared.act_scope())?
            else {
                let context = control.new_diagnostic_context(
                    prepared.identity().clone(),
                    None,
                    ToolPayloadCapturePolicy::default(),
                );
                control
                    .install_diagnostic_context(context)
                    .map_err(|error| {
                        self.state
                            .failure_owner
                            .latch_state_failure("act.production-context-attach-failed");
                        PyRuntimeError::new_err(format!(
                            "attach mandatory Production diagnostic context: {error}"
                        ))
                    })?;
                prepared.commit();
                return self.bind_usage(&act_id);
            };
            let reservation = self.state.registry.reserve(act_id.as_str())?;
            let coordinator = ActSettlementCoordinator::new();
            coordinator
                .install_authority_expiry(prepared_authority.expiry())
                .map_err(|error| PyRuntimeError::new_err(error.to_string()))?;
            let subscriber = Arc::new(AuthorityOnlySubscriber {
                state: Arc::downgrade(&self.state),
                act_id: act_id.clone(),
                authority: prepared_authority.authority(),
                coordinator,
            });
            prepared_authority.stage(py)?;
            let context = control.new_diagnostic_context(
                prepared.identity().clone(),
                None,
                ToolPayloadCapturePolicy::default(),
            );
            control
                .install_diagnostic_context(context)
                .map_err(|error| {
                    self.state
                        .failure_owner
                        .latch_state_failure("act.production-context-attach-failed");
                    PyRuntimeError::new_err(format!(
                        "attach mandatory Production diagnostic context: {error}"
                    ))
                })?;
            reservation.publish(Arc::clone(&subscriber));
            prepared.commit();
            prepared_authority.commit();
            self.bind_usage(&act_id)
        }

        fn bind_usage(&self, act_id: &RunLocalId) -> PyResult<()> {
            let (late_code, failed_code, label) = match self.state.profile {
                DiagnosticAdmissionProfile::ProductionDurable => (
                    "act.production-usage-bind-late",
                    "act.production-usage-bind-failed",
                    "mandatory Production",
                ),
                DiagnosticAdmissionProfile::SinkOnlyVolatile => (
                    "act.standalone-usage-bind-late",
                    "act.standalone-usage-bind-failed",
                    "standalone",
                ),
            };
            match self.state.usage.bind_act(act_id.as_str()) {
                Ok(UsageObservationDisposition::LateIgnored) => {
                    self.state.failure_owner.latch_state_failure(late_code);
                    Err(PyRuntimeError::new_err(format!(
                        "{label} diagnostic usage binding missed the admitted Act"
                    )))
                }
                Ok(_) => Ok(()),
                Err(error) => {
                    self.state.failure_owner.latch_state_failure(failed_code);
                    Err(PyRuntimeError::new_err(format!(
                        "bind {label} diagnostic usage: {error}"
                    )))
                }
            }
        }
    }

    impl DiagnosticAdmissionCapability for ActSinkAdmissionCapability {
        fn as_any(&self) -> &dyn Any {
            self
        }

        fn profile(&self) -> DiagnosticAdmissionProfile {
            self.state.profile
        }

        fn traverse(&self, visit: &PyVisit<'_>) -> Result<(), PyTraverseError> {
            self.state.registry.traverse(visit)
        }

        fn admit_act(
            &self,
            py: Python<'_>,
            run: &RunBinding,
            cued: &Arc<CuedScope>,
            control: &Arc<AgentTurnControl>,
            binding: DiagnosticActBinding,
        ) -> PyResult<()> {
            if !binding.is_active() {
                if self.state.profile == DiagnosticAdmissionProfile::ProductionDurable {
                    return self.admit_without_sink(py, run, cued, control);
                }
                return Ok(());
            }
            self.bind(py, run, cued, control, binding)
        }
    }

    struct CapabilityState {
        profile: DiagnosticAdmissionProfile,
        run_id: CanonicalUuid,
        context: DiagnosticRunContext,
        registry: Arc<ActSinkRegistry>,
        failure_owner: Arc<dyn ActDiagnosticFailureOwner>,
        usage: Arc<UsageFinalizingObservationBridge>,
        standalone: Option<StandaloneResources>,
        sink_runtime: Mutex<Option<DiagnosticSinkRuntime>>,
    }

    impl CapabilityState {
        fn register_sink(
            &self,
            py: Python<'_>,
            act_id: RunLocalId,
            callback: Py<PyAny>,
        ) -> Result<SinkHandle, String> {
            let mut runtime = lock(&self.sink_runtime);
            if runtime.is_none() {
                let started = py
                    .detach(DiagnosticSinkRuntime::start)
                    .map_err(|error| error.to_string())?;
                *runtime = Some(started);
            }
            runtime
                .as_ref()
                .expect("the sink Runtime was initialized")
                .register_sink(self.run_id, act_id, callback)
                .map_err(|error| error.to_string())
        }
    }

    impl Drop for CapabilityState {
        fn drop(&mut self) {
            let Some(runtime) = self
                .sink_runtime
                .get_mut()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .take()
            else {
                return;
            };
            let bound = self.registry.bound_all();
            for subscriber in &bound {
                if let Err(error) = subscriber.settlement.begin_runtime_shutdown() {
                    self.failure_owner.latch_state_failure(error.code());
                }
            }
            let deadline = Instant::now() + BINDING_SHUTDOWN_GRACE;
            Python::attach(|py| {
                let _report = py.detach(|| runtime.shutdown_until(deadline));
            });
            for subscriber in bound {
                if let Err(error) = subscriber.settlement.publish_latched_summary() {
                    self.failure_owner.latch_state_failure(error.code());
                }
            }
        }
    }

    struct StandaloneResources {
        observer: AgentDiagnosticObserver,
        lineages: Mutex<StandaloneLineages>,
    }

    #[derive(Default)]
    struct StandaloneLineages {
        next_scene: u64,
        next_actor: u64,
        next_cue: u64,
        scenes: HashMap<usize, RunLocalId>,
        actors: HashMap<usize, StandaloneActor>,
        cues: HashMap<usize, RunLocalId>,
    }

    #[derive(Clone)]
    struct StandaloneActor {
        actor_id: RunLocalId,
        session_id: RunLocalId,
    }

    struct StandaloneLineage {
        cue_scope: DiagnosticScope,
        session: AgentSessionDiagnosticContext,
    }

    impl StandaloneLineages {
        fn resolve(&mut self, cued: &Arc<CuedScope>) -> Result<StandaloneLineage, &'static str> {
            let scene = cued.scene();
            let scene_key = Arc::as_ptr(&scene) as usize;
            let scene_id = match self.scenes.get(&scene_key) {
                Some(value) => value.clone(),
                None => {
                    let value = next_lineage_id("standalone-scene", &mut self.next_scene)?;
                    self.scenes.insert(scene_key, value.clone());
                    value
                }
            };
            let actor_key = cued.actor_identity();
            let actor = match self.actors.get(&actor_key) {
                Some(value) => value.clone(),
                None => {
                    let actor_id = next_lineage_id("standalone-actor", &mut self.next_actor)?;
                    let session_id =
                        RunLocalId::parse(&format!("standalone-session-{}", self.next_actor))
                            .map_err(|_| "act.standalone-session-identifier-invalid")?;
                    let value = StandaloneActor {
                        actor_id,
                        session_id,
                    };
                    self.actors.insert(actor_key, value.clone());
                    value
                }
            };
            let cue_key = Arc::as_ptr(cued) as usize;
            let cue_id = match self.cues.get(&cue_key) {
                Some(value) => value.clone(),
                None => {
                    let value = next_lineage_id("standalone-cue", &mut self.next_cue)?;
                    self.cues.insert(cue_key, value.clone());
                    value
                }
            };
            Ok(StandaloneLineage {
                cue_scope: DiagnosticScope::new(
                    Some(scene_id),
                    Some(actor.actor_id.clone()),
                    Some(cue_id),
                    None,
                    None,
                    None,
                    None,
                ),
                session: AgentSessionDiagnosticContext::new(
                    actor.actor_id.as_str(),
                    actor.session_id.as_str(),
                ),
            })
        }
    }

    fn next_lineage_id(
        prefix: &'static str,
        counter: &mut u64,
    ) -> Result<RunLocalId, &'static str> {
        *counter = counter
            .checked_add(1)
            .ok_or("act.standalone-identifier-exhausted")?;
        RunLocalId::parse(&format!("{prefix}-{counter}"))
            .map_err(|_| "act.standalone-identifier-invalid")
    }

    #[derive(Default)]
    struct StandaloneFailureOwner {
        first_failure: OnceLock<String>,
    }

    impl ActDiagnosticFailureOwner for StandaloneFailureOwner {
        fn latch_diagnostic_failure(&self, error: DiagnosticProducerError) {
            let _ = self.first_failure.set(error.to_string());
        }

        fn latch_state_failure(&self, code: &'static str) {
            let _ = self.first_failure.set(code.to_owned());
        }
    }

    impl AgentDiagnosticFailureOwner for StandaloneFailureOwner {
        fn observer_failed(&self, failure: AgentDiagnosticObserverFailure) {
            let _ = self.first_failure.set(format!(
                "{}:{}",
                failure.observation_kind().as_str(),
                failure.error_code().as_str()
            ));
        }
    }

    impl UsageFinalizationFailureOwner for StandaloneFailureOwner {
        fn usage_finalization_failed(&self, act_id: &str, error_code: AgentDiagnosticErrorCode) {
            let _ = self
                .first_failure
                .set(format!("{act_id}:{}", error_code.as_str()));
        }
    }

    #[derive(Default)]
    struct ActSinkRegistry {
        entries: Mutex<HashMap<String, RegistryEntry>>,
    }

    enum RegistryEntry {
        Reserved,
        Bound(Arc<ActSinkSubscriber>),
        Authority(Arc<AuthorityOnlySubscriber>),
    }

    impl ActSinkRegistry {
        fn reserve(&self, act_id: &str) -> PyResult<SubscriberReservation<'_>> {
            let mut entries = lock(&self.entries);
            if entries.contains_key(act_id) {
                return Err(PyRuntimeError::new_err(
                    "diagnostic Act subscriber identity is already registered",
                ));
            }
            entries.insert(act_id.to_owned(), RegistryEntry::Reserved);
            Ok(SubscriberReservation {
                registry: self,
                act_id: act_id.to_owned(),
                published: false,
            })
        }

        fn bound(&self, act_id: &str) -> Option<Arc<ActSinkSubscriber>> {
            match lock(&self.entries).get(act_id) {
                Some(RegistryEntry::Bound(value)) => Some(Arc::clone(value)),
                Some(RegistryEntry::Reserved | RegistryEntry::Authority(_)) | None => None,
            }
        }

        fn bound_all(&self) -> Vec<Arc<ActSinkSubscriber>> {
            lock(&self.entries)
                .values()
                .filter_map(|entry| match entry {
                    RegistryEntry::Bound(value) => Some(Arc::clone(value)),
                    RegistryEntry::Reserved | RegistryEntry::Authority(_) => None,
                })
                .collect()
        }

        fn subscriber(&self, act_id: &str) -> Option<Arc<dyn ActEventSubscriber>> {
            match lock(&self.entries).get(act_id) {
                Some(RegistryEntry::Bound(value)) => {
                    Some(Arc::clone(value) as Arc<dyn ActEventSubscriber>)
                }
                Some(RegistryEntry::Authority(value)) => {
                    Some(Arc::clone(value) as Arc<dyn ActEventSubscriber>)
                }
                Some(RegistryEntry::Reserved) | None => None,
            }
        }

        fn contains_bound_sink(&self, act_id: &str, sink_id: u64) -> bool {
            matches!(
                lock(&self.entries).get(act_id),
                Some(RegistryEntry::Bound(subscriber)) if subscriber.handle.id() == sink_id
            )
        }

        fn retire_expected(&self, act_id: &str, sink_id: u64) -> bool {
            let mut entries = lock(&self.entries);
            let expected = matches!(
                entries.get(act_id),
                Some(RegistryEntry::Bound(subscriber)) if subscriber.handle.id() == sink_id
            );
            if expected {
                let removed = entries.remove(act_id);
                debug_assert!(matches!(removed, Some(RegistryEntry::Bound(_))));
            }
            expected
        }

        fn retire_authority_expected(&self, act_id: &str, generation: u64) -> bool {
            let mut entries = lock(&self.entries);
            let expected = matches!(
                entries.get(act_id),
                Some(RegistryEntry::Authority(subscriber))
                    if subscriber.authority.generation() == generation
            );
            if expected {
                let removed = entries.remove(act_id);
                debug_assert!(matches!(removed, Some(RegistryEntry::Authority(_))));
            }
            expected
        }

        fn traverse(&self, visit: &PyVisit<'_>) -> Result<(), PyTraverseError> {
            for entry in lock(&self.entries).values() {
                if let RegistryEntry::Bound(subscriber) = entry {
                    visit.call(&subscriber.request)?;
                }
            }
            Ok(())
        }
    }

    impl DiagnosticActSubscriberLookup for ActSinkRegistry {
        fn subscriber_for(&self, act_id: &str) -> Option<Arc<dyn ActEventSubscriber>> {
            self.subscriber(act_id)
        }

        fn deliver_tool_payload(
            &self,
            act_id: &str,
            canonical_tool_call_id: &str,
            payload: &SinkOnlyToolPayload,
        ) {
            if let Some(subscriber) = self.bound(act_id) {
                subscriber.deliver_tool_payload(canonical_tool_call_id, payload);
            }
        }
    }

    struct SubscriberReservation<'a> {
        registry: &'a ActSinkRegistry,
        act_id: String,
        published: bool,
    }

    impl From<Arc<ActSinkSubscriber>> for RegistryEntry {
        fn from(value: Arc<ActSinkSubscriber>) -> Self {
            Self::Bound(value)
        }
    }

    impl From<Arc<AuthorityOnlySubscriber>> for RegistryEntry {
        fn from(value: Arc<AuthorityOnlySubscriber>) -> Self {
            Self::Authority(value)
        }
    }

    impl SubscriberReservation<'_> {
        fn publish(mut self, subscriber: impl Into<RegistryEntry>) {
            let replaced =
                lock(&self.registry.entries).insert(self.act_id.clone(), subscriber.into());
            debug_assert!(matches!(replaced, Some(RegistryEntry::Reserved)));
            self.published = true;
        }
    }

    impl Drop for SubscriberReservation<'_> {
        fn drop(&mut self) {
            if !self.published {
                let removed = lock(&self.registry.entries).remove(&self.act_id);
                debug_assert!(matches!(removed, Some(RegistryEntry::Reserved)));
            }
        }
    }

    struct AuthorityOnlySubscriber {
        state: Weak<CapabilityState>,
        act_id: RunLocalId,
        authority: custom_act_binding::ActAuthority,
        coordinator: Arc<ActSettlementCoordinator>,
    }

    impl ActEventSubscriber for AuthorityOnlySubscriber {
        fn deliver(&self, event: AcceptedDiagnosticEvent) -> Result<(), DeliveryFailure> {
            self.authority.observe(&event);
            if !matches!(event.event(), DiagnosticEvent::SpanFinished(_))
                || event.built_in_span_kind() != Some(SpanKind::ActLifecycle)
            {
                return Ok(());
            }
            let Some(state) = self.state.upgrade() else {
                return Err(DeliveryFailure::new(SINK_SETTLEMENT_FAILED));
            };
            if let Err(error) = self.coordinator.settle_authority_only() {
                state.failure_owner.latch_state_failure(error.code());
                return Err(DeliveryFailure::new(SINK_SETTLEMENT_FAILED));
            }
            if !state
                .registry
                .retire_authority_expected(self.act_id.as_str(), self.authority.generation())
            {
                state
                    .failure_owner
                    .latch_state_failure("act.authority-retire-missing");
                return Err(DeliveryFailure::new(SINK_SETTLEMENT_FAILED));
            }
            Ok(())
        }
    }

    struct ActSinkSubscriber {
        state: Weak<CapabilityState>,
        context: DiagnosticRunContext,
        capture: DiagnosticCaptureConfig,
        act_scope: DiagnosticScope,
        authority: Option<custom_act_binding::ActAuthority>,
        request: Py<PyAny>,
        handle: SinkHandle,
        payload: Mutex<PayloadState>,
        drop_facts: Mutex<DropFactState>,
        failure_facts: Arc<DeliveryFactEmitter>,
        settlement: Arc<ActSinkSettlement>,
        terminal_enqueued: AtomicBool,
    }

    impl ActSinkSubscriber {
        fn deliver_tool_payload(
            &self,
            canonical_tool_call_id: &str,
            payload: &SinkOnlyToolPayload,
        ) {
            let mut state = lock(&self.payload);
            let payload =
                prepare_sink_tool_payload(canonical_tool_call_id, payload, &mut state.budget);
            state.pending.push(payload);
        }

        fn take_payload(&self, event: &DiagnosticEvent) -> Option<PreparedSinkToolPayload> {
            let (tool_call_id, source) = tool_payload_key(event)?;
            let mut payload = lock(&self.payload);
            let index = payload.pending.iter().position(|candidate| {
                candidate.tool_call_id() == tool_call_id && candidate.source() == source
            })?;
            Some(payload.pending.remove(index))
        }

        fn deliver_projected(
            &self,
            canonical: AcceptedDiagnosticEvent,
        ) -> Result<(), DeliveryFailure> {
            let payload = self.take_payload(canonical.event());
            let Some(projected) =
                project_act_event(&canonical, &self.act_scope, self.capture, payload.as_ref())
            else {
                return Ok(());
            };
            let source_gaps = usize::from(matches!(
                projected.event(),
                DiagnosticEvent::ObservationGap(_)
            ));
            let truncated_payloads = projected
                .captured_input()
                .into_iter()
                .filter(|input| input.truncated())
                .count()
                + projected
                    .captured_output()
                    .into_iter()
                    .filter(|output| output.truncated())
                    .count();
            if source_gaps != 0 {
                let _ = self.handle.record_source_gaps(source_gaps);
            }
            if truncated_payloads != 0 {
                let _ = self.handle.record_truncated_payloads(truncated_payloads);
            }

            let sequence = projected.canonical().identity().sequence();
            let subscriber_local = projected.canonical().is_subscriber_local();
            let event_kind = projected.event().kind();
            let is_drop_counter = matches!(
                projected.event(),
                DiagnosticEvent::CounterSampled(event)
                    if event.counter_kind() == CounterKind::DiagnosticDroppedEvents
            );
            let act_outcome = match projected.event() {
                DiagnosticEvent::SpanFinished(event)
                    if projected.canonical().built_in_span_kind()
                        == Some(SpanKind::ActLifecycle) =>
                {
                    Some(match event.outcome() {
                        SpanOutcome::Completed => ActOutcome::Completed,
                        SpanOutcome::Cancelled => ActOutcome::Cancelled,
                        SpanOutcome::Failed => ActOutcome::Failed,
                    })
                }
                _ => None,
            };
            let materialized = Python::attach(|py| materialize_projected_event(py, &projected));
            let (event, sidecar_bytes) = match materialized {
                Ok(value) => value,
                Err(_) => {
                    self.failure_facts.report_enqueue(Some(sequence));
                    return Err(DeliveryFailure::new(SINK_DELIVERY_FAILED));
                }
            };
            let encoded_bytes = projected
                .canonical_bytes()
                .len()
                .saturating_add(sidecar_bytes);
            let dispatch = DispatchEvent::new(sequence.get(), event);
            let outcome = if act_outcome.is_some() {
                let outcome = self
                    .handle
                    .try_enqueue_terminal(dispatch, event_kind, encoded_bytes);
                if outcome.is_ok() {
                    self.terminal_enqueued.store(true, Ordering::Release);
                }
                outcome
            } else {
                self.handle.try_enqueue(
                    dispatch,
                    event_kind,
                    encoded_bytes,
                    if is_drop_counter {
                        AdmissionClass::Structural
                    } else {
                        AdmissionClass::Content
                    },
                )
            };
            let outcome = match outcome {
                Ok(outcome) => outcome,
                Err(_) => {
                    self.failure_facts.report_enqueue(Some(sequence));
                    return Err(DeliveryFailure::new(SINK_DELIVERY_FAILED));
                }
            };
            if !is_drop_counter {
                self.record_drops(sequence, subscriber_local, &outcome);
            }
            if let Some(act_outcome) = act_outcome {
                self.settle_terminal(act_outcome)?;
            }
            Ok(())
        }

        fn settle_terminal(&self, outcome: ActOutcome) -> Result<(), DeliveryFailure> {
            let Some(state) = self.state.upgrade() else {
                return Err(DeliveryFailure::new(SINK_SETTLEMENT_FAILED));
            };
            let sink = BoundSinkSettlement {
                subscriber: self,
                state: &state,
                outcome,
            };
            if let Err(error) = self.settlement.coordinator().settle_with_sink(&sink) {
                state.failure_owner.latch_state_failure(error.code());
                return Err(DeliveryFailure::new(SINK_SETTLEMENT_FAILED));
            }
            Ok(())
        }

        #[allow(dead_code)] // Installed before subscriber publication.
        fn install_authority_expiry(
            &self,
            authority: Arc<dyn ActAuthorityExpiry>,
        ) -> Result<(), ActAuthorityExpiryInstallError> {
            self.settlement.install_authority_expiry(authority)
        }

        fn record_drops(
            &self,
            source_sequence: SchemaU64,
            subscriber_local: bool,
            outcome: &AdmissionOutcome,
        ) {
            let dropped = dropped_events(outcome);
            if dropped == 0 {
                return;
            }
            let Some(state) = self.state.upgrade() else {
                return;
            };
            let mut facts = lock(&self.drop_facts);
            facts.cumulative = facts.cumulative.saturating_add(dropped as u64);
            if subscriber_local {
                return;
            }
            let value = SchemaU64::new(facts.cumulative);
            let result = self.context.emit_counter_without_act_subscriber(
                self.act_scope.clone(),
                CounterKind::DiagnosticDroppedEvents,
                value,
                vec![CausalLink::new(
                    source_sequence,
                    CausalRelation::FollowsFrom,
                )],
            );
            if let Err(error) = result {
                state.failure_owner.latch_diagnostic_failure(error);
            }
        }
    }

    struct BoundSinkSettlement<'a> {
        subscriber: &'a ActSinkSubscriber,
        state: &'a CapabilityState,
        outcome: ActOutcome,
    }

    impl ActSettlementSink for BoundSinkSettlement<'_> {
        fn prepare_settlement(&self) -> Result<(), ActSettlementError> {
            self.subscriber.settlement.prepare_terminal_seal()?;
            let act_id = self
                .subscriber
                .act_scope
                .act_id()
                .expect("an Act sink settlement has an Act ID");
            if !self
                .state
                .registry
                .contains_bound_sink(act_id.as_str(), self.subscriber.handle.id())
            {
                return Err(ActSettlementError::new(
                    "act.sink-retire-missing",
                    "diagnostic sink binding is missing before terminal settlement",
                ));
            }
            Ok(())
        }

        fn commit_settlement(&self) -> ActSettlementSinkCommit {
            let mut committed_failure = match self
                .subscriber
                .settlement
                .commit_terminal_seal(self.outcome)
            {
                ActSettlementSinkCommit::Rejected(error) => {
                    return ActSettlementSinkCommit::Rejected(error);
                }
                ActSettlementSinkCommit::Committed => None,
                ActSettlementSinkCommit::CommittedWithFailure(error) => Some(error),
            };
            let act_id = self
                .subscriber
                .act_scope
                .act_id()
                .expect("an Act sink settlement has an Act ID");
            if !self
                .state
                .registry
                .retire_expected(act_id.as_str(), self.subscriber.handle.id())
            {
                committed_failure.get_or_insert_with(|| {
                    ActSettlementError::new(
                        "act.sink-retire-missing",
                        "diagnostic sink binding disappeared during terminal settlement",
                    )
                });
            }
            self.subscriber.settlement.start_close_waiter();
            match committed_failure {
                Some(error) => ActSettlementSinkCommit::CommittedWithFailure(error),
                None => ActSettlementSinkCommit::Committed,
            }
        }
    }

    impl ActEventSubscriber for ActSinkSubscriber {
        fn deliver(&self, event: AcceptedDiagnosticEvent) -> Result<(), DeliveryFailure> {
            if let Some(authority) = &self.authority {
                authority.observe(&event);
            }
            self.deliver_projected(event)
        }
    }

    #[derive(Default)]
    struct PayloadState {
        budget: AgentToolPayloadActBudget,
        pending: Vec<PreparedSinkToolPayload>,
    }

    #[derive(Default)]
    struct DropFactState {
        cumulative: u64,
    }

    fn tool_payload_key(event: &DiagnosticEvent) -> Option<(&str, ToolPayloadSource)> {
        let source = match event {
            DiagnosticEvent::SpanStarted(event) if event.span_kind() == SpanKind::ToolCall => {
                ToolPayloadSource::Started
            }
            DiagnosticEvent::InstantOccurred(event)
                if event.instant_kind() == InstantKind::ToolUpdated =>
            {
                ToolPayloadSource::Updated
            }
            _ => return None,
        };
        Some((event.header().scope().tool_call_id()?.as_str(), source))
    }

    fn dropped_events(outcome: &AdmissionOutcome) -> usize {
        match outcome {
            AdmissionOutcome::Enqueued { evicted } => evicted
                .iter()
                .fold(0_usize, |total, delta| total.saturating_add(delta.events())),
            AdmissionOutcome::Dropped { delta, .. } => delta.events(),
            AdmissionOutcome::Terminalized { dropped } => dropped
                .iter()
                .fold(0_usize, |total, delta| total.saturating_add(delta.events())),
        }
    }

    struct DeliveryFactEmitter {
        state: Weak<CapabilityState>,
        context: DiagnosticRunContext,
        scope: DiagnosticScope,
        component_id: RunLocalId,
        emitted: AtomicBool,
    }

    impl DeliveryFactEmitter {
        fn new(
            state: Weak<CapabilityState>,
            context: DiagnosticRunContext,
            scope: DiagnosticScope,
            sink_id: u64,
        ) -> Self {
            Self {
                state,
                context,
                scope,
                component_id: RunLocalId::parse(&format!("sink-{sink_id}"))
                    .expect("a numeric sink ID is a RunLocalId"),
                emitted: AtomicBool::new(false),
            }
        }

        fn report_enqueue(&self, related: Option<SchemaU64>) {
            self.report(
                ComponentFailureStage::Enqueue,
                ComponentFailureErrorCode::DeliveryQueueUnavailable,
                related,
            );
        }

        fn report(
            &self,
            stage: ComponentFailureStage,
            error_code: ComponentFailureErrorCode,
            related: Option<SchemaU64>,
        ) {
            if self
                .emitted
                .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
                .is_err()
            {
                return;
            }
            let Some(state) = self.state.upgrade() else {
                return;
            };
            let detail = DiagnosticComponentFailedDetail::new(
                self.component_id.clone(),
                stage,
                error_code,
                related,
            )
            .expect("the sink delivery failure mapping is closed");
            if let Err(error) = self.context.emit_instant_without_act_subscriber(
                self.scope.clone(),
                InstantDetail::DiagnosticComponentFailed(detail),
                None,
                Vec::new(),
            ) {
                state.failure_owner.latch_diagnostic_failure(error);
            }
        }
    }

    impl DeliveryFactEmitter {
        fn delivery_failed(&self, failure: SinkDeliveryFailure) {
            match failure {
                SinkDeliveryFailure::Callback(failure) => {
                    let error_code = match failure.kind().as_str() {
                        "raised" => ComponentFailureErrorCode::CallbackRaised,
                        "invalid_return" => ComponentFailureErrorCode::CallbackInvalidReturn,
                        _ => unreachable!("callback failure kinds are closed"),
                    };
                    self.report(
                        ComponentFailureStage::Callback,
                        error_code,
                        Some(SchemaU64::new(failure.event_sequence())),
                    );
                }
                SinkDeliveryFailure::Unexpected(_) => self.report_enqueue(None),
            }
        }
    }

    pub(crate) struct BoundActSink {
        subscriber: Arc<ActSinkSubscriber>,
    }

    impl BoundActSink {
        pub(crate) fn handle(&self) -> SinkHandle {
            self.subscriber.handle.clone()
        }

        pub(crate) fn request(&self, py: Python<'_>) -> Py<PyAny> {
            self.subscriber.request.clone_ref(py)
        }

        pub(crate) fn capture(&self) -> DiagnosticCaptureConfig {
            self.subscriber.capture
        }

        pub(crate) fn act_scope(&self) -> &DiagnosticScope {
            &self.subscriber.act_scope
        }

        pub(crate) fn terminal_enqueued(&self) -> bool {
            self.subscriber.terminal_enqueued.load(Ordering::Acquire)
        }

        #[allow(dead_code)] // Installed during the admission transaction.
        pub(crate) fn install_authority_expiry(
            &self,
            authority: Arc<dyn ActAuthorityExpiry>,
        ) -> Result<(), ActAuthorityExpiryInstallError> {
            self.subscriber.install_authority_expiry(authority)
        }
    }

    pub(crate) fn bound_sink_for(run: &RunBinding, act_id: &str) -> Option<BoundActSink> {
        let capability = run.diagnostic_admission().capability()?;
        let capability = capability
            .as_any()
            .downcast_ref::<ActSinkAdmissionCapability>()?;
        capability
            .state
            .registry
            .bound(act_id)
            .map(|subscriber| BoundActSink { subscriber })
    }

    #[derive(Clone, Copy, Debug)]
    struct StandaloneReserver;

    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    struct StandaloneReserveError;

    struct StandaloneReservation;

    impl AdmissionReserver for StandaloneReserver {
        type Error = StandaloneReserveError;
        type Reservation = StandaloneReservation;

        fn try_reserve(&mut self, size: AdmissionSize) -> Result<Self::Reservation, Self::Error> {
            if size.event_count() != 1 || size.canonical_bytes() > STANDALONE_EVENT_MAX_BYTES {
                return Err(StandaloneReserveError);
            }
            Ok(StandaloneReservation)
        }
    }

    impl BoundedInMemoryReserver for StandaloneReserver {}

    impl AdmissionReservation for StandaloneReservation {
        fn commit(self, _event: AcceptedDiagnosticEvent) {}
    }

    impl fmt::Display for StandaloneReserveError {
        fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            formatter.write_str("standalone diagnostic event exceeds its in-memory bound")
        }
    }

    impl std::error::Error for StandaloneReserveError {}

    fn admission_state_error(code: &'static str) -> PyErr {
        PyRuntimeError::new_err(format!("diagnostic sink admission failed [{code}]"))
    }

    fn diagnostics_module(py: Python<'_>) -> PyResult<Bound<'_, PyModule>> {
        py.import("troupe.diagnostics")
    }

    fn diagnostic_sink_state_error(diagnostics: &Bound<'_, PyModule>, code: &'static str) -> PyErr {
        let kwargs = PyDict::new(diagnostics.py());
        if let Err(error) = kwargs.set_item("code", code) {
            return error;
        }
        diagnostics
            .getattr("DiagnosticSinkStateError")
            .and_then(|error_type| error_type.call((), Some(&kwargs)))
            .map_or_else(|error| error, PyErr::from_value)
    }

    fn require_base_slot<'py>(
        diagnostics: &Bound<'py, PyModule>,
        base_type: &Bound<'py, PyType>,
        instance: &Bound<'py, PyAny>,
        slot: &str,
    ) -> PyResult<Bound<'py, PyAny>> {
        let descriptor = base_type.getattr("__dict__")?.get_item(slot)?;
        match descriptor.call_method1("__get__", (instance, instance.get_type())) {
            Ok(value) => Ok(value),
            Err(error) if error.is_instance_of::<PyAttributeError>(diagnostics.py()) => {
                Err(diagnostic_sink_state_error(diagnostics, "uninitialized"))
            }
            Err(error) => Err(error),
        }
    }

    struct PythonLockGuard {
        lock: Py<PyAny>,
    }

    impl PythonLockGuard {
        fn new(lock: Py<PyAny>) -> Self {
            Self { lock }
        }
    }

    impl Drop for PythonLockGuard {
        fn drop(&mut self) {
            Python::attach(|py| {
                let _ = self.lock.bind(py).call_method0("release");
            });
        }
    }

    fn materialize_projected_event(
        py: Python<'_>,
        projected: &SinkProjectedEvent,
    ) -> PyResult<(Py<PyAny>, usize)> {
        let diagnostics = diagnostics_module(py)?;
        let canonical = pyo3::types::PyBytes::new(py, projected.canonical_bytes());
        let event = diagnostics
            .getattr("_event_from_json_bytes")?
            .call1((canonical,))?;
        if projected.captured_input().is_none() && projected.captured_output().is_none() {
            return Ok((event.unbind(), 0));
        }

        let json = diagnostics.getattr("_json")?;
        let freeze = diagnostics.getattr("_freeze_json")?;
        let sidecar = PyDict::new(py);
        let captured_input = match projected.captured_input() {
            Some(input) => {
                let mapping = PyDict::new(py);
                let raw_input = parse_projected_json(&json, &freeze, input.raw_input())?;
                match &raw_input {
                    Some((native, _)) => mapping.set_item("raw_input", native)?,
                    None => mapping.set_item("raw_input", py.None())?,
                }
                mapping.set_item("truncated", input.truncated())?;
                sidecar.set_item("captured_input", &mapping)?;
                let kwargs = PyDict::new(py);
                match raw_input {
                    Some((_, frozen)) => kwargs.set_item("raw_input", frozen)?,
                    None => kwargs.set_item("raw_input", py.None())?,
                }
                kwargs.set_item("truncated", input.truncated())?;
                Some(
                    diagnostics
                        .getattr("DiagnosticToolInput")?
                        .call((), Some(&kwargs))?
                        .unbind(),
                )
            }
            None => None,
        };
        let captured_output = match projected.captured_output() {
            Some(output) => {
                let mapping = PyDict::new(py);
                let raw_output = parse_projected_json(&json, &freeze, output.raw_output())?;
                match &raw_output {
                    Some((native, _)) => mapping.set_item("raw_output", native)?,
                    None => mapping.set_item("raw_output", py.None())?,
                }
                let mut content = Vec::with_capacity(output.content().len());
                let mut frozen_content = Vec::with_capacity(output.content().len());
                for value in output.content() {
                    let parsed = json.call_method1("loads", (value.canonical_json(),))?;
                    frozen_content.push(freeze.call1((&parsed,))?.unbind());
                    content.push(parsed.unbind());
                }
                let content_mapping = PyTuple::new(py, content.iter())?;
                mapping.set_item("content", &content_mapping)?;
                let mut locations = Vec::with_capacity(output.locations().len());
                let mut location_mappings = Vec::with_capacity(output.locations().len());
                for location in output.locations() {
                    let location_mapping = PyDict::new(py);
                    location_mapping.set_item("path", location.path())?;
                    match location.line() {
                        Some(line) => location_mapping.set_item("line", line)?,
                        None => location_mapping.set_item("line", py.None())?,
                    }
                    location_mappings.push(location_mapping.unbind());
                    let kwargs = PyDict::new(py);
                    kwargs.set_item("path", location.path())?;
                    match location.line() {
                        Some(line) => kwargs.set_item("line", line)?,
                        None => kwargs.set_item("line", py.None())?,
                    }
                    locations.push(
                        diagnostics
                            .getattr("DiagnosticToolLocation")?
                            .call((), Some(&kwargs))?
                            .unbind(),
                    );
                }
                let location_mapping_tuple = PyTuple::new(py, location_mappings.iter())?;
                mapping.set_item("locations", &location_mapping_tuple)?;
                mapping.set_item("truncated", output.truncated())?;
                sidecar.set_item("captured_output", &mapping)?;

                let kwargs = PyDict::new(py);
                match raw_output {
                    Some((_, frozen)) => kwargs.set_item("raw_output", frozen)?,
                    None => kwargs.set_item("raw_output", py.None())?,
                }
                kwargs.set_item("content", PyTuple::new(py, frozen_content.iter())?)?;
                kwargs.set_item("locations", PyTuple::new(py, locations.iter())?)?;
                kwargs.set_item("truncated", output.truncated())?;
                Some(
                    diagnostics
                        .getattr("DiagnosticToolOutput")?
                        .call((), Some(&kwargs))?
                        .unbind(),
                )
            }
            None => None,
        };

        let dumps_kwargs = PyDict::new(py);
        dumps_kwargs.set_item("ensure_ascii", false)?;
        dumps_kwargs.set_item("allow_nan", false)?;
        dumps_kwargs.set_item("separators", (",", ":"))?;
        let sidecar_bytes = json
            .call_method("dumps", (&sidecar,), Some(&dumps_kwargs))?
            .extract::<String>()?
            .len();
        let replace = py.import("dataclasses")?.getattr("replace")?;
        let detail_kwargs = PyDict::new(py);
        if let Some(input) = captured_input {
            detail_kwargs.set_item("captured_input", input)?;
        }
        if let Some(output) = captured_output {
            detail_kwargs.set_item("captured_output", output)?;
        }
        let detail = replace.call((event.getattr("detail")?,), Some(&detail_kwargs))?;
        let event_kwargs = PyDict::new(py);
        event_kwargs.set_item("detail", detail)?;
        let projected = replace.call((&event,), Some(&event_kwargs))?;
        Ok((projected.unbind(), sidecar_bytes))
    }

    fn parse_projected_json(
        json: &Bound<'_, PyAny>,
        freeze: &Bound<'_, PyAny>,
        value: Option<&SinkProjectedJsonValue>,
    ) -> PyResult<Option<(Py<PyAny>, Py<PyAny>)>> {
        value
            .map(|value| {
                let parsed = json.call_method1("loads", (value.canonical_json(),))?;
                let frozen = freeze.call1((&parsed,))?.unbind();
                Ok((parsed.unbind(), frozen))
            })
            .transpose()
    }
}
