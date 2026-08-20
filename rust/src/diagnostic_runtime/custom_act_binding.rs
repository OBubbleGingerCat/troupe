#[cfg(not(test))]
use std::fmt;
#[cfg(not(test))]
use std::sync::atomic::{AtomicU64, Ordering};
#[cfg(not(test))]
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, Weak};

#[cfg(not(test))]
use pyo3::exceptions::PyRuntimeError;
#[cfg(not(test))]
use pyo3::prelude::*;
#[cfg(not(test))]
use pyo3::types::{PyAny, PyAnyMethods, PyDict, PyWeakrefMethods, PyWeakrefReference};
#[cfg(not(test))]
use troupe_diagnostics_core::event::{DiagnosticEvent, DiagnosticScope};
#[cfg(not(test))]
use troupe_diagnostics_core::hub::AcceptedDiagnosticEvent;
#[cfg(not(test))]
use troupe_diagnostics_core::id::RunLocalId;
#[cfg(not(test))]
use troupe_diagnostics_core::kinds::SpanKind;

#[cfg(not(test))]
use crate::diagnostic_runtime::act_producer;
#[cfg(not(test))]
use crate::diagnostic_runtime::custom_binding::{
    CustomDomainExtension, CustomDomainSnapshot, install_domain_extension,
};
#[cfg(not(test))]
use crate::diagnostic_runtime::runtime_producer::{self, RuntimeLifecycleProducer};
#[cfg(not(test))]
use crate::diagnostic_runtime::sink_settlement::{
    ActAuthorityExpiry, ActAuthorityExpiryPrepareError, PreparedActAuthorityExpiry,
};
#[cfg(not(test))]
use crate::orchestration::python_task::TaskLineage;
#[cfg(not(test))]
use crate::orchestration::scene_context::{CuedScope, RunBinding};

#[cfg(not(test))]
const ACT_CONTEXT_ERROR: &str = "custom diagnostic publication requires an active Act authority";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ActAuthorityRole {
    Caller,
    CallerDescendant,
    Supervisor,
}

#[derive(Clone)]
pub(crate) struct ActTaskAuthority {
    generation: u64,
    role: ActAuthorityRole,
    #[cfg(not(test))]
    authority: ActAuthority,
}

impl ActTaskAuthority {
    pub(crate) const fn generation(&self) -> u64 {
        self.generation
    }

    pub(crate) fn for_registered_child(&self) -> Self {
        Self {
            generation: self.generation,
            role: match self.role {
                ActAuthorityRole::Caller | ActAuthorityRole::CallerDescendant => {
                    ActAuthorityRole::CallerDescendant
                }
                ActAuthorityRole::Supervisor => ActAuthorityRole::Supervisor,
            },
            #[cfg(not(test))]
            authority: self.authority.clone(),
        }
    }

    pub(crate) fn is_supervisor(&self) -> bool {
        self.role == ActAuthorityRole::Supervisor
    }

    pub(crate) fn active_supervisor(&self) -> bool {
        if !self.is_supervisor() {
            return false;
        }
        #[cfg(not(test))]
        {
            self.authority.generation_is_active(self.generation)
        }
        #[cfg(test)]
        false
    }

    #[cfg(not(test))]
    fn generation_is_active(&self) -> bool {
        self.authority.generation_is_active(self.generation)
    }

    #[cfg(not(test))]
    fn resolve(&self, py: Python<'_>, binding: &Arc<RunBinding>) -> PyResult<CustomDomainSnapshot> {
        self.authority
            .resolve(py, binding, self.generation, self.role)
    }

    #[cfg(not(test))]
    #[allow(dead_code)] // Reserved for an explicitly registered Runtime supervisor task.
    pub(crate) fn for_supervisor(&self) -> Self {
        Self {
            generation: self.generation,
            role: ActAuthorityRole::Supervisor,
            authority: self.authority.clone(),
        }
    }
}

#[cfg(not(test))]
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[cfg(not(test))]
fn diagnostic_context_error(py: Python<'_>, message: &'static str) -> PyErr {
    let value = (|| {
        let modules = py
            .import("sys")?
            .getattr("modules")?
            .cast_into::<PyDict>()?;
        let diagnostics = modules
            .get_item("troupe.diagnostics")?
            .ok_or_else(|| PyRuntimeError::new_err("troupe.diagnostics is not installed"))?;
        diagnostics
            .getattr("DiagnosticContextError")?
            .call1((message,))
    })();
    value.map_or_else(|error| error, PyErr::from_value)
}

#[cfg(not(test))]
#[derive(Clone, Copy)]
struct AuthorityPhase {
    caller_active: bool,
    generation_active: bool,
}

#[cfg(not(test))]
struct ActAuthorityInner {
    binding: Weak<RunBinding>,
    runtime: Weak<RuntimeLifecycleProducer>,
    act_id: RunLocalId,
    act_scope: DiagnosticScope,
    generation: u64,
    // A callback-free weakref is non-owning and cannot create a Python reference cycle.
    caller_task: Py<PyWeakrefReference>,
    caller_base_lineage: TaskLineage,
    phase: Mutex<AuthorityPhase>,
}

#[cfg(not(test))]
#[derive(Clone)]
pub(crate) struct ActAuthority {
    inner: Arc<ActAuthorityInner>,
}

#[cfg(not(test))]
impl ActAuthority {
    fn token(&self, role: ActAuthorityRole) -> ActTaskAuthority {
        ActTaskAuthority {
            generation: self.inner.generation,
            role,
            authority: self.clone(),
        }
    }

    pub(crate) fn generation(&self) -> u64 {
        self.inner.generation
    }

    fn generation_is_active(&self, generation: u64) -> bool {
        generation == self.inner.generation && lock(&self.inner.phase).generation_active
    }

    fn resolve(
        &self,
        py: Python<'_>,
        binding: &Arc<RunBinding>,
        generation: u64,
        role: ActAuthorityRole,
    ) -> PyResult<CustomDomainSnapshot> {
        {
            let phase = lock(&self.inner.phase);
            let caller_role = matches!(
                role,
                ActAuthorityRole::Caller | ActAuthorityRole::CallerDescendant
            );
            if generation != self.inner.generation
                || !phase.generation_active
                || (caller_role && !phase.caller_active)
            {
                return Err(diagnostic_context_error(py, ACT_CONTEXT_ERROR));
            }
        }

        let Some(expected_binding) = self.inner.binding.upgrade() else {
            return Err(diagnostic_context_error(py, ACT_CONTEXT_ERROR));
        };
        if !Arc::ptr_eq(&expected_binding, binding) {
            return Err(diagnostic_context_error(py, ACT_CONTEXT_ERROR));
        }
        let Some(runtime) = self.inner.runtime.upgrade() else {
            return Err(diagnostic_context_error(py, ACT_CONTEXT_ERROR));
        };
        let Some(current_runtime) = runtime_producer::producer_for_binding(binding) else {
            return Err(diagnostic_context_error(py, ACT_CONTEXT_ERROR));
        };
        if !Arc::ptr_eq(&runtime, &current_runtime) {
            runtime.latch_state_failure("custom.act-authority-runtime-mismatch");
            return Err(PyRuntimeError::new_err(
                "custom diagnostic Act authority resolved another Runtime",
            ));
        }
        let Some(snapshot) = act_producer::lineage_snapshot(self.inner.act_id.as_str()) else {
            return Err(diagnostic_context_error(py, ACT_CONTEXT_ERROR));
        };
        if snapshot.act_scope() != &self.inner.act_scope {
            runtime.latch_state_failure("custom.act-authority-scope-mismatch");
            return Err(PyRuntimeError::new_err(
                "custom diagnostic Act authority resolved another Act scope",
            ));
        }
        Ok(CustomDomainSnapshot::new(
            runtime,
            snapshot.context(),
            snapshot.event_scope().clone(),
            snapshot.containing_span_id(),
        ))
    }

    pub(crate) fn observe(&self, canonical: &AcceptedDiagnosticEvent) {
        let event = canonical.event();
        let observed_act = event.header().scope().act_id();
        if observed_act != Some(&self.inner.act_id) {
            if let Some(runtime) = self.inner.runtime.upgrade() {
                runtime.latch_state_failure("custom.act-authority-event-scope-mismatch");
            }
            return;
        }
        if matches!(event, DiagnosticEvent::SpanFinished(_))
            && canonical.built_in_span_kind() == Some(SpanKind::ActCaller)
        {
            lock(&self.inner.phase).caller_active = false;
        }
    }

    fn update_caller_lineage(
        &self,
        expected_generation: Option<u64>,
        replacement: TaskLineage,
    ) -> CallerLineageUpdate {
        let Some(binding) = self.inner.binding.upgrade() else {
            return CallerLineageUpdate::Gone;
        };
        Python::attach(|py| {
            let Some(task) = self.inner.caller_task.bind(py).upgrade() else {
                return CallerLineageUpdate::Gone;
            };
            if binding.replace_task_lineage_if(&task, expected_generation, replacement) {
                CallerLineageUpdate::Updated
            } else {
                CallerLineageUpdate::Mismatch
            }
        })
    }
}

#[cfg(not(test))]
enum CallerLineageUpdate {
    Updated,
    Gone,
    Mismatch,
}

#[cfg(not(test))]
impl ActAuthorityExpiry for ActAuthority {
    fn prepare_expiry(
        &self,
    ) -> Result<Box<dyn PreparedActAuthorityExpiry>, ActAuthorityExpiryPrepareError> {
        if !lock(&self.inner.phase).generation_active {
            return Err(ActAuthorityExpiryPrepareError::new(
                "act.authority-generation-expired",
            ));
        }
        Ok(Box::new(PreparedGenerationExpiry {
            authority: self.clone(),
            previous: None,
            caller_lineage_cleared: false,
        }))
    }
}

#[cfg(not(test))]
struct PreparedGenerationExpiry {
    authority: ActAuthority,
    previous: Option<AuthorityPhase>,
    caller_lineage_cleared: bool,
}

#[cfg(not(test))]
impl PreparedActAuthorityExpiry for PreparedGenerationExpiry {
    fn commit(&mut self) {
        let previous = {
            let mut phase = lock(&self.authority.inner.phase);
            let previous = *phase;
            phase.caller_active = false;
            phase.generation_active = false;
            previous
        };
        self.previous = Some(previous);
        let replacement = self.authority.inner.caller_base_lineage.clone();
        match self
            .authority
            .update_caller_lineage(Some(self.authority.inner.generation), replacement)
        {
            CallerLineageUpdate::Updated => self.caller_lineage_cleared = true,
            CallerLineageUpdate::Gone => {}
            CallerLineageUpdate::Mismatch => {
                if let Some(runtime) = self.authority.inner.runtime.upgrade() {
                    runtime.latch_state_failure("custom.act-authority-expiry-lineage-mismatch");
                }
            }
        }
    }

    fn rollback(&mut self) {
        let Some(previous) = self.previous.take() else {
            return;
        };
        *lock(&self.authority.inner.phase) = previous;
        if self.caller_lineage_cleared {
            let replacement = self
                .authority
                .inner
                .caller_base_lineage
                .with_act_authority(self.authority.token(ActAuthorityRole::Caller));
            if matches!(
                self.authority.update_caller_lineage(None, replacement),
                CallerLineageUpdate::Mismatch
            ) && let Some(runtime) = self.authority.inner.runtime.upgrade()
            {
                runtime.latch_state_failure("custom.act-authority-rollback-lineage-mismatch");
            }
        }
    }
}

#[cfg(not(test))]
struct ActDomainExtension;

#[cfg(not(test))]
impl CustomDomainExtension for ActDomainExtension {
    fn resolve(
        &self,
        py: Python<'_>,
        binding: &Arc<RunBinding>,
        lineage: &TaskLineage,
    ) -> PyResult<Option<CustomDomainSnapshot>> {
        lineage
            .act_authority()
            .map(|authority| authority.resolve(py, binding))
            .transpose()
    }
}

#[cfg(not(test))]
fn ensure_domain_extension() -> PyResult<()> {
    static INSTALL: OnceLock<Result<(), String>> = OnceLock::new();
    match INSTALL.get_or_init(|| {
        let extension: Arc<dyn CustomDomainExtension> = Arc::new(ActDomainExtension);
        install_domain_extension(extension).map_err(|error| error.to_string())
    }) {
        Ok(()) => Ok(()),
        Err(error) => Err(PyRuntimeError::new_err(error.clone())),
    }
}

#[cfg(not(test))]
fn next_generation(runtime: &RuntimeLifecycleProducer) -> PyResult<u64> {
    static NEXT_GENERATION: AtomicU64 = AtomicU64::new(1);
    NEXT_GENERATION
        .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |generation| {
            generation.checked_add(1)
        })
        .map_err(|_| {
            runtime.latch_state_failure("custom.act-authority-generation-exhausted");
            PyRuntimeError::new_err("custom diagnostic Act authority generation is exhausted")
        })
}

#[cfg(not(test))]
pub(crate) struct PreparedActAuthority {
    binding: Weak<RunBinding>,
    task: Py<PyAny>,
    original: TaskLineage,
    authority: ActAuthority,
    staged: bool,
    committed: bool,
}

#[cfg(not(test))]
impl PreparedActAuthority {
    pub(crate) fn authority(&self) -> ActAuthority {
        self.authority.clone()
    }

    pub(crate) fn expiry(&self) -> Arc<dyn ActAuthorityExpiry> {
        Arc::new(self.authority.clone())
    }

    pub(crate) fn stage(&mut self, py: Python<'_>) -> PyResult<()> {
        if self.staged {
            return Err(PyRuntimeError::new_err(
                "custom diagnostic Act authority was staged twice",
            ));
        }
        let Some(binding) = self.binding.upgrade() else {
            return Err(diagnostic_context_error(py, ACT_CONTEXT_ERROR));
        };
        let expected = self
            .original
            .act_authority()
            .map(ActTaskAuthority::generation);
        let replacement = self
            .original
            .without_act_authority()
            .with_act_authority(self.authority.token(ActAuthorityRole::Caller));
        if !binding.replace_task_lineage_if(self.task.bind(py), expected, replacement) {
            return Err(diagnostic_context_error(py, ACT_CONTEXT_ERROR));
        }
        self.staged = true;
        Ok(())
    }

    pub(crate) fn commit(mut self) {
        debug_assert!(self.staged, "Act authority must be staged before commit");
        self.committed = true;
    }
}

#[cfg(not(test))]
impl Drop for PreparedActAuthority {
    fn drop(&mut self) {
        if !self.staged || self.committed {
            return;
        }
        let Some(binding) = self.binding.upgrade() else {
            return;
        };
        Python::attach(|py| {
            if !binding.replace_task_lineage_if(
                self.task.bind(py),
                Some(self.authority.generation()),
                self.original.clone(),
            ) && let Some(runtime) = self.authority.inner.runtime.upgrade()
            {
                runtime.latch_state_failure("custom.act-authority-admission-rollback-mismatch");
            }
        });
    }
}

#[cfg(not(test))]
pub(crate) fn prepare(
    py: Python<'_>,
    binding: &RunBinding,
    cued: &Arc<CuedScope>,
    act_scope: &DiagnosticScope,
) -> PyResult<Option<PreparedActAuthority>> {
    let Some(runtime) = runtime_producer::producer_for_binding(binding) else {
        return Ok(None);
    };
    let binding = cued
        .scene()
        .binding()
        .filter(|candidate| std::ptr::eq(candidate.as_ref(), binding))
        .ok_or_else(|| diagnostic_context_error(py, ACT_CONTEXT_ERROR))?;
    ensure_domain_extension()?;
    let lineage = binding
        .current_lineage(py)?
        .filter(TaskLineage::is_active)
        .ok_or_else(|| diagnostic_context_error(py, ACT_CONTEXT_ERROR))?;
    let matches_cued = lineage
        .cued()
        .is_some_and(|current| Arc::ptr_eq(&current, cued));
    if !matches_cued {
        return Err(diagnostic_context_error(py, ACT_CONTEXT_ERROR));
    }
    if lineage
        .act_authority()
        .is_some_and(ActTaskAuthority::generation_is_active)
    {
        return Err(diagnostic_context_error(py, ACT_CONTEXT_ERROR));
    }
    let task = binding
        .current_task(py)?
        .ok_or_else(|| diagnostic_context_error(py, ACT_CONTEXT_ERROR))?;
    let caller_task = PyWeakrefReference::new(&task)?.unbind();
    let act_id = act_scope
        .act_id()
        .cloned()
        .ok_or_else(|| PyRuntimeError::new_err("diagnostic Act scope has no Act ID"))?;
    let generation = next_generation(&runtime)?;
    let caller_base_lineage = lineage.without_act_authority();
    let authority = ActAuthority {
        inner: Arc::new(ActAuthorityInner {
            binding: Arc::downgrade(&binding),
            runtime: Arc::downgrade(&runtime),
            act_id,
            act_scope: act_scope.clone(),
            generation,
            caller_task,
            caller_base_lineage,
            phase: Mutex::new(AuthorityPhase {
                caller_active: true,
                generation_active: true,
            }),
        }),
    };
    Ok(Some(PreparedActAuthority {
        binding: Arc::downgrade(&binding),
        task: task.unbind(),
        original: lineage,
        authority,
        staged: false,
        committed: false,
    }))
}

#[cfg(not(test))]
impl fmt::Debug for ActAuthority {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ActAuthority")
            .field("act_id", &self.inner.act_id.as_str())
            .field("generation", &self.inner.generation)
            .finish_non_exhaustive()
    }
}
