use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard};

use tokio::sync::Notify;

use crate::adapter::agent_adapter;
use crate::diagnostics::observer::{AgentDiagnosticObserver, AgentDiagnosticObserverInstallError};
use crate::diagnostics::session::AgentSessionDiagnosticContext;
use crate::error::AgentStartupFailure;
use crate::launch::{NpxPreparationKey, ResolvedLaunch, ResolvedLaunchKind, resolve_launch};
use crate::profile::ResolvedAgentProfile;
use crate::result::ResultMcpService;
use crate::session::{AgentSessionSlot, NpxPreparationGate, spawn_opening};

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

pub struct AgentSupervisor {
    result_service: Arc<ResultMcpService>,
    control: Arc<SupervisorControl>,
}

struct SupervisorControl {
    state: Mutex<SupervisorState>,
    changed: Notify,
}

struct SupervisorState {
    shutting_down: bool,
    session_opening_started: bool,
    diagnostic_observer: Option<AgentDiagnosticObserver>,
    active_casts: usize,
    sessions: Vec<Arc<AgentSessionSlot>>,
    package_preparations: HashMap<NpxPreparationKey, Arc<NpxPreparationGate>>,
}

pub struct AgentCastPermit {
    control: Arc<SupervisorControl>,
}

impl AgentSupervisor {
    pub fn new() -> Self {
        Self {
            result_service: ResultMcpService::new(),
            control: Arc::new(SupervisorControl {
                state: Mutex::new(SupervisorState {
                    shutting_down: false,
                    session_opening_started: false,
                    diagnostic_observer: None,
                    active_casts: 0,
                    sessions: Vec::new(),
                    package_preparations: HashMap::new(),
                }),
                changed: Notify::new(),
            }),
        }
    }

    pub fn begin_cast(&self) -> Result<AgentCastPermit, AgentStartupFailure> {
        let mut state = lock(&self.control.state);
        if state.shutting_down {
            return Err(AgentStartupFailure::start(
                "preparation_failed",
                "preparation",
                "Production is shutting down",
            ));
        }
        state.active_casts += 1;
        drop(state);
        Ok(AgentCastPermit {
            control: Arc::clone(&self.control),
        })
    }

    pub fn install_diagnostic_observer(
        &self,
        observer: AgentDiagnosticObserver,
    ) -> Result<(), AgentDiagnosticObserverInstallError> {
        let mut state = lock(&self.control.state);
        if state.session_opening_started {
            return Err(AgentDiagnosticObserverInstallError::SessionOpeningStarted);
        }
        if state.diagnostic_observer.is_some() {
            return Err(AgentDiagnosticObserverInstallError::AlreadyInstalled);
        }
        state.diagnostic_observer = Some(observer);
        Ok(())
    }

    pub fn resolve(
        &self,
        profile: &ResolvedAgentProfile,
    ) -> Result<ResolvedLaunch, AgentStartupFailure> {
        resolve_launch(profile.agent)
    }

    pub fn start(
        &self,
        permit: &AgentCastPermit,
        profile: Arc<ResolvedAgentProfile>,
        launch: ResolvedLaunch,
    ) -> Arc<AgentSessionSlot> {
        self.start_inner(permit, profile, launch, None)
    }

    pub fn start_with_diagnostic_context(
        &self,
        permit: &AgentCastPermit,
        profile: Arc<ResolvedAgentProfile>,
        launch: ResolvedLaunch,
        diagnostic_context: AgentSessionDiagnosticContext,
    ) -> Arc<AgentSessionSlot> {
        self.start_inner(permit, profile, launch, Some(diagnostic_context))
    }

    fn start_inner(
        &self,
        permit: &AgentCastPermit,
        profile: Arc<ResolvedAgentProfile>,
        launch: ResolvedLaunch,
        diagnostic_context: Option<AgentSessionDiagnosticContext>,
    ) -> Arc<AgentSessionSlot> {
        assert!(
            Arc::ptr_eq(&self.control, &permit.control),
            "an agent cast permit belongs to its Production"
        );
        let diagnostic_observer = {
            let mut state = lock(&self.control.state);
            state.session_opening_started = true;
            state.diagnostic_observer.clone()
        };
        #[cfg(feature = "agent-test-support")]
        if matches!(launch.0, ResolvedLaunchKind::Inert) {
            let slot = AgentSessionSlot::inert(
                Arc::clone(&profile),
                diagnostic_observer,
                diagnostic_context,
            );
            return slot;
        }
        let command = match launch.0 {
            ResolvedLaunchKind::Process(command) => command,
            ResolvedLaunchKind::Inert => {
                unreachable!("inert launch returned after its early branch")
            }
        };
        let spec = agent_adapter(profile.agent).launch_spec();
        let package_preparation = spec
            .npx_preparation_key()
            .map(|key| self.package_preparation(key));
        let slot = AgentSessionSlot::new_with_session_diagnostics(
            diagnostic_observer,
            diagnostic_context,
            Arc::clone(&profile),
        );
        slot.observe_opening();
        #[cfg(feature = "agent-test-support")]
        slot.install_test_turn_registration_gate(command.turn_gates.registration.clone());
        #[cfg(feature = "agent-test-support")]
        slot.install_test_turn_terminal_delivery_gate(command.turn_gates.terminal_delivery.clone());
        self.track(&slot);
        let control = Arc::downgrade(&self.control);
        spawn_opening(
            &slot,
            profile,
            spec,
            *command,
            Arc::clone(&self.result_service),
            package_preparation,
            Box::new(move |slot| {
                if let Some(control) = control.upgrade() {
                    control.release(&slot);
                }
            }),
        );
        slot
    }

    fn track(&self, slot: &Arc<AgentSessionSlot>) {
        let mut state = lock(&self.control.state);
        state
            .sessions
            .retain(|session| !session.cleanup_is_complete());
        state.sessions.push(Arc::clone(slot));
    }

    fn package_preparation(&self, key: NpxPreparationKey) -> Arc<NpxPreparationGate> {
        Arc::clone(
            lock(&self.control.state)
                .package_preparations
                .entry(key)
                .or_insert_with(NpxPreparationGate::new),
        )
    }

    fn begin_shutdown(&self) -> Vec<Arc<AgentSessionSlot>> {
        let mut state = lock(&self.control.state);
        state.shutting_down = true;
        let sessions = state.sessions.clone();
        drop(state);
        self.control.changed.notify_waiters();
        for session in &sessions {
            session.cancel();
        }
        sessions
    }

    pub async fn shutdown_and_wait(&self) {
        {
            let mut state = lock(&self.control.state);
            state.shutting_down = true;
        }
        self.control.changed.notify_waiters();
        loop {
            let changed = self.control.changed.notified();
            if lock(&self.control.state).active_casts == 0 {
                break;
            }
            changed.await;
        }
        let sessions = lock(&self.control.state).sessions.clone();
        for session in &sessions {
            session.cancel();
        }
        for session in &sessions {
            session.wait_cleanup().await;
        }
        self.result_service.shutdown_and_wait().await;
    }

    #[cfg(feature = "agent-test-support")]
    pub fn is_shutting_down(&self) -> bool {
        lock(&self.control.state).shutting_down
    }

    #[cfg(feature = "agent-test-support")]
    pub fn fail_result_listener_for_test(&self) {
        self.result_service.fail_listener_for_test();
    }

    #[cfg(feature = "agent-test-support")]
    pub fn tracked_session_count(&self) -> usize {
        lock(&self.control.state).sessions.len()
    }
}

impl Default for AgentSupervisor {
    fn default() -> Self {
        Self::new()
    }
}

impl SupervisorControl {
    fn release(&self, slot: &Arc<AgentSessionSlot>) {
        lock(&self.state)
            .sessions
            .retain(|tracked| !Arc::ptr_eq(tracked, slot));
        self.changed.notify_waiters();
    }
}

impl Drop for AgentCastPermit {
    fn drop(&mut self) {
        let mut state = lock(&self.control.state);
        state.active_casts = state
            .active_casts
            .checked_sub(1)
            .expect("an agent cast permit is released once");
        drop(state);
        self.control.changed.notify_waiters();
    }
}

impl Drop for AgentSupervisor {
    fn drop(&mut self) {
        let sessions = self.begin_shutdown();
        let result_service = Arc::clone(&self.result_service);
        pyo3_async_runtimes::tokio::get_runtime().spawn(async move {
            for session in sessions {
                session.wait_cleanup().await;
            }
            result_service.shutdown_and_wait().await;
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Destination;

    #[test]
    fn diagnostic_observer_installation_is_one_shot_and_pre_opening() {
        let supervisor = AgentSupervisor::new();
        let observer = AgentDiagnosticObserver::from_destination(Arc::new(Destination));
        assert_eq!(
            supervisor.install_diagnostic_observer(observer.clone()),
            Ok(())
        );
        assert_eq!(
            supervisor.install_diagnostic_observer(observer.clone()),
            Err(AgentDiagnosticObserverInstallError::AlreadyInstalled)
        );

        lock(&supervisor.control.state).session_opening_started = true;
        assert_eq!(
            supervisor.install_diagnostic_observer(observer),
            Err(AgentDiagnosticObserverInstallError::SessionOpeningStarted)
        );
    }
}
