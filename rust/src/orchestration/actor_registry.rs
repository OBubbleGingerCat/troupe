use std::collections::HashMap;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, Weak};

use pyo3::exceptions::{PyRuntimeError, PyValueError};
use pyo3::prelude::*;
use pyo3::types::PyString;

use crate::agent::{
    AgentCastPermit, AgentSessionSlot, AgentStartupFailure, AgentSupervisor, ResolvedAgentProfile,
    ResolvedLaunch,
};
use crate::diagnostic_runtime::actor_producer::{self, ActorHook};
use crate::orchestration::actor::{ActorCapability, ActorIdentity};
use crate::orchestration::cue::CueContextError;
use crate::orchestration::scene_context::RunBinding;

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub(crate) struct NameKey(Vec<u32>);

impl NameKey {
    pub(crate) fn from_python(name: &Bound<'_, PyString>) -> PyResult<Self> {
        let py = name.py();
        let length = unsafe { pyo3::ffi::PyUnicode_GetLength(name.as_ptr()) };
        if length < 0 {
            return Err(PyErr::fetch(py));
        }

        let mut code_points = Vec::with_capacity(length as usize);
        for index in 0..length {
            let value = unsafe { pyo3::ffi::PyUnicode_ReadChar(name.as_ptr(), index) };
            if value == u32::MAX {
                return Err(PyErr::fetch(py));
            }
            code_points.push(value);
        }
        Ok(Self(code_points))
    }

    #[cfg(test)]
    fn ascii(value: &str) -> Self {
        Self(value.chars().map(u32::from).collect())
    }

    pub(crate) fn is_reserved_scene_name(&self) -> bool {
        const PREFIX: &[u32] = &[
            b's' as u32,
            b'c' as u32,
            b'e' as u32,
            b'n' as u32,
            b'e' as u32,
            b'-' as u32,
        ];
        if self.0.len() != PREFIX.len() + 36 || !self.0.starts_with(PREFIX) {
            return false;
        }

        self.0[PREFIX.len()..]
            .iter()
            .enumerate()
            .all(|(index, value)| {
                if matches!(index, 8 | 13 | 18 | 23) {
                    *value == b'-' as u32
                } else {
                    matches!(*value, 0x30..=0x39 | 0x61..=0x66)
                }
            })
    }
}

enum RegistryEntry<T> {
    Reserved(Arc<ActorIdentity>),
    Live {
        identity: Weak<ActorIdentity>,
        capability: Weak<T>,
    },
}

struct ActorRegistry<T> {
    entries: HashMap<NameKey, RegistryEntry<T>>,
}

impl<T> Default for ActorRegistry<T> {
    fn default() -> Self {
        Self {
            entries: HashMap::new(),
        }
    }
}

impl<T> ActorRegistry<T> {
    fn reserve(&mut self, key: NameKey, identity: Arc<ActorIdentity>) -> bool {
        if self.entries.contains_key(&key) {
            return false;
        }
        self.entries.insert(key, RegistryEntry::Reserved(identity));
        true
    }

    fn commit(&mut self, key: &NameKey, identity: &Arc<ActorIdentity>, capability: &Arc<T>) {
        assert!(
            matches!(
                self.entries.get(key),
                Some(RegistryEntry::Reserved(current)) if Arc::ptr_eq(current, identity)
            ),
            "a reservation must remain owned until it is committed"
        );
        self.entries.insert(
            key.clone(),
            RegistryEntry::Live {
                identity: Arc::downgrade(identity),
                capability: Arc::downgrade(capability),
            },
        );
    }

    fn rollback(&mut self, key: &NameKey, identity: &Arc<ActorIdentity>) {
        let should_remove = matches!(
            self.entries.get(key),
            Some(RegistryEntry::Reserved(current)) if Arc::ptr_eq(current, identity)
        );
        if should_remove {
            self.entries.remove(key);
        }
    }

    fn detach(&mut self, key: &NameKey, identity: &Arc<ActorIdentity>) {
        let should_remove = matches!(
            self.entries.get(key),
            Some(RegistryEntry::Live { identity: current, .. })
                if Weak::ptr_eq(current, &Arc::downgrade(identity))
        );
        if should_remove {
            self.entries.remove(key);
        }
    }

    fn get(&mut self, key: &NameKey) -> Option<Arc<T>> {
        let capability = match self.entries.get(key) {
            Some(RegistryEntry::Live { capability, .. }) => capability.upgrade(),
            _ => return None,
        };
        if capability.is_none() {
            self.entries.remove(key);
        }
        capability
    }

    fn snapshot(&mut self) -> Vec<Arc<T>> {
        let mut capabilities = Vec::new();
        self.entries.retain(|_, entry| match entry {
            RegistryEntry::Reserved(_) => true,
            RegistryEntry::Live { capability, .. } => {
                if let Some(capability) = capability.upgrade() {
                    capabilities.push(capability);
                    true
                } else {
                    false
                }
            }
        });
        capabilities
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

pub(crate) struct RegistryReservation<T> {
    registry: Arc<Mutex<ActorRegistry<T>>>,
    key: NameKey,
    identity: Arc<ActorIdentity>,
    committed: bool,
}

impl<T> RegistryReservation<T> {
    fn reserve(
        registry: Arc<Mutex<ActorRegistry<T>>>,
        key: NameKey,
        identity: Arc<ActorIdentity>,
    ) -> Result<Self, ()> {
        if !lock(&registry).reserve(key.clone(), Arc::clone(&identity)) {
            return Err(());
        }
        Ok(Self {
            registry,
            key,
            identity,
            committed: false,
        })
    }

    pub(crate) fn identity(&self) -> &Arc<ActorIdentity> {
        &self.identity
    }

    pub(crate) fn key(&self) -> &NameKey {
        &self.key
    }

    pub(crate) fn commit(mut self, capability: &Arc<T>) {
        lock(&self.registry).commit(&self.key, &self.identity, capability);
        actor_producer::observe_identity(None, &self.identity, None, ActorHook::RegistryCommitted);
        self.committed = true;
    }
}

impl<T> Drop for RegistryReservation<T> {
    fn drop(&mut self) {
        if !self.committed {
            lock(&self.registry).rollback(&self.key, &self.identity);
        }
    }
}

pub(crate) struct ProductionState {
    registry: Arc<Mutex<ActorRegistry<ActorCapability>>>,
    agent_supervisor: AgentSupervisor,
    pub(crate) active: Mutex<Weak<RunBinding>>,
    pub(crate) owner_pid: AtomicU32,
}

impl ProductionState {
    pub(crate) fn new() -> Self {
        Self {
            registry: Arc::new(Mutex::new(ActorRegistry::default())),
            agent_supervisor: AgentSupervisor::new(),
            active: Mutex::new(Weak::new()),
            owner_pid: AtomicU32::new(std::process::id()),
        }
    }

    pub(crate) fn reserve_name(
        &self,
        name: &Bound<'_, PyString>,
    ) -> PyResult<RegistryReservation<ActorCapability>> {
        let key = NameKey::from_python(name)?;
        if key.is_reserved_scene_name() {
            let name_repr = python_str_repr(name)?;
            return Err(PyValueError::new_err(format!(
                "Actor name is reserved for scene identities: {name_repr}"
            )));
        }

        match RegistryReservation::reserve(Arc::clone(&self.registry), key, Arc::new(ActorIdentity))
        {
            Ok(reservation) => {
                actor_producer::observe_identity(
                    Some(self),
                    reservation.identity(),
                    Some(name),
                    ActorHook::RegistryReserved,
                );
                Ok(reservation)
            }
            Err(()) => {
                let name_repr = python_str_repr(name)?;
                Err(PyValueError::new_err(format!(
                    "Actor name is already in use: {name_repr}"
                )))
            }
        }
    }

    pub(crate) fn get(&self, key: &NameKey) -> Option<Arc<ActorCapability>> {
        lock(&self.registry).get(key)
    }

    pub(crate) fn snapshot(&self) -> Vec<Arc<ActorCapability>> {
        lock(&self.registry).snapshot()
    }

    pub(crate) fn detach(&self, key: &NameKey, identity: &Arc<ActorIdentity>) {
        lock(&self.registry).detach(key, identity);
        actor_producer::observe_identity(Some(self), identity, None, ActorHook::RegistryDetached);
    }

    pub(crate) fn resolve_agent_launch(
        &self,
        profile: &ResolvedAgentProfile,
    ) -> Result<ResolvedLaunch, AgentStartupFailure> {
        self.agent_supervisor.resolve(profile)
    }

    pub(crate) fn begin_agent_cast(&self) -> Result<AgentCastPermit, AgentStartupFailure> {
        if !self.pid_matches() {
            return Err(AgentStartupFailure::start(
                "preparation_failed",
                "preparation",
                "Production belongs to another process",
            ));
        }
        self.agent_supervisor.begin_cast()
    }

    pub(crate) fn start_agent_session(
        &self,
        permit: &AgentCastPermit,
        profile: Arc<ResolvedAgentProfile>,
        launch: ResolvedLaunch,
    ) -> Arc<AgentSessionSlot> {
        self.agent_supervisor.start(permit, profile, launch)
    }

    pub(crate) async fn shutdown_agent_sessions(&self) {
        self.agent_supervisor.shutdown_and_wait().await;
    }

    #[cfg(feature = "agent-test-support")]
    pub(crate) fn agent_sessions_are_shutting_down(&self) -> bool {
        self.agent_supervisor.is_shutting_down()
    }

    #[cfg(feature = "agent-test-support")]
    pub(crate) fn fail_agent_result_listener_for_test(&self) {
        self.agent_supervisor.fail_result_listener_for_test();
    }

    pub(crate) fn ensure_owner_process(&self) -> PyResult<()> {
        if self.pid_matches() {
            Ok(())
        } else {
            Err(PyRuntimeError::new_err(
                "Production belongs to another process",
            ))
        }
    }

    #[cfg(feature = "agent-test-support")]
    pub(crate) fn tracked_agent_session_count(&self) -> usize {
        self.agent_supervisor.tracked_session_count()
    }

    fn pid_matches(&self) -> bool {
        self.owner_pid.load(Ordering::Acquire) == std::process::id()
    }

    pub(crate) fn active_binding_for_cue(&self) -> PyResult<Arc<RunBinding>> {
        if !self.pid_matches() {
            return Err(CueContextError::new_err(
                "ActorHandle.cue() must be called within an active scene context",
            ));
        }
        lock(&self.active).upgrade().ok_or_else(|| {
            CueContextError::new_err(
                "ActorHandle.cue() must be called within an active scene context",
            )
        })
    }

    pub(crate) fn bind(self: &Arc<Self>, binding: &Arc<RunBinding>) -> PyResult<()> {
        if !self.pid_matches() {
            return Err(PyRuntimeError::new_err(
                "Production belongs to another process",
            ));
        }
        if !binding.production_matches(self) {
            return Err(PyRuntimeError::new_err(
                "Runtime binding belongs to another Production",
            ));
        }
        let mut active = lock(&self.active);
        if active.upgrade().is_some() {
            return Err(PyRuntimeError::new_err(
                "Production is already bound to an active runtime",
            ));
        }
        *active = Arc::downgrade(binding);
        Ok(())
    }

    #[cfg(test)]
    pub(crate) fn bind_for_test(&self, binding: &Arc<RunBinding>) -> PyResult<()> {
        if !self.pid_matches() {
            return Err(PyRuntimeError::new_err(
                "Production belongs to another process",
            ));
        }
        let mut active = lock(&self.active);
        if active.upgrade().is_some() {
            return Err(PyRuntimeError::new_err(
                "Production is already bound to an active runtime",
            ));
        }
        *active = Arc::downgrade(binding);
        Ok(())
    }

    #[cfg(test)]
    pub(crate) fn active_binding_for_test(&self) -> Option<Arc<RunBinding>> {
        lock(&self.active).upgrade()
    }
}

fn python_str_repr(name: &Bound<'_, PyString>) -> PyResult<String> {
    name.py()
        .get_type::<PyString>()
        .getattr("__repr__")?
        .call1((name,))?
        .extract()
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex, Weak, atomic::Ordering, mpsc};
    use std::time::Duration;

    use crate::orchestration::scene_context::RunBinding;

    use super::{ActorIdentity, ActorRegistry, NameKey, RegistryReservation, lock};

    #[test]
    fn reservation_guard_rolls_back_until_committed() {
        let registry = Arc::new(Mutex::new(ActorRegistry::<()>::default()));
        let key = NameKey::ascii("actor");

        {
            let reservation = RegistryReservation::reserve(
                Arc::clone(&registry),
                key.clone(),
                Arc::new(ActorIdentity),
            )
            .expect("the first reservation must succeed");
            assert!(lock(&registry).get(&key).is_none());
            drop(reservation);
        }

        let reservation = RegistryReservation::reserve(
            Arc::clone(&registry),
            key.clone(),
            Arc::new(ActorIdentity),
        )
        .expect("rollback must release the name");
        let capability = Arc::new(());
        reservation.commit(&capability);
        assert!(Arc::ptr_eq(
            &lock(&registry).get(&key).expect("commit must publish"),
            &capability
        ));
    }

    #[test]
    fn registry_entry_does_not_own_capability() {
        let registry = Arc::new(Mutex::new(ActorRegistry::<()>::default()));
        let key = NameKey::ascii("weak");
        let reservation = RegistryReservation::reserve(
            Arc::clone(&registry),
            key.clone(),
            Arc::new(ActorIdentity),
        )
        .expect("reservation must succeed");
        let capability = Arc::new(());
        reservation.commit(&capability);

        assert_eq!(Arc::strong_count(&capability), 1);
        drop(capability);
        assert!(lock(&registry).get(&key).is_none());
    }

    #[test]
    fn stale_cleanup_cannot_remove_same_name_successor() {
        let mut registry = ActorRegistry::<()>::default();
        let key = NameKey::ascii("aba");
        let old_identity = Arc::new(ActorIdentity);
        let old_capability = Arc::new(());
        assert!(registry.reserve(key.clone(), Arc::clone(&old_identity)));
        registry.commit(&key, &old_identity, &old_capability);
        registry.detach(&key, &old_identity);

        let new_identity = Arc::new(ActorIdentity);
        let new_capability = Arc::new(());
        assert!(registry.reserve(key.clone(), Arc::clone(&new_identity)));
        registry.commit(&key, &new_identity, &new_capability);
        registry.detach(&key, &old_identity);

        assert!(Arc::ptr_eq(
            &registry.get(&key).expect("successor must remain indexed"),
            &new_capability
        ));
    }

    #[test]
    fn active_slot_is_a_weak_run_binding() {
        let state = super::ProductionState::new();
        let _: &Mutex<Weak<RunBinding>> = &state.active;
    }

    #[test]
    fn pid_mismatch_is_rejected_before_all_production_state_mutexes() {
        let state = Arc::new(super::ProductionState::new());
        state
            .owner_pid
            .store(std::process::id().wrapping_add(1), Ordering::Release);
        let registry_guard = super::lock(&state.registry);
        let active_guard = super::lock(&state.active);
        let (sender, receiver) = mpsc::channel();
        let probe_state = Arc::clone(&state);
        let probe = std::thread::spawn(move || {
            sender
                .send(probe_state.active_binding_for_cue().is_err())
                .expect("the PID probe receiver must remain alive");
        });

        let result = receiver.recv_timeout(Duration::from_secs(1));
        drop(active_guard);
        drop(registry_guard);
        probe.join().expect("the PID probe must not panic");
        assert_eq!(result, Ok(true));
    }
}
