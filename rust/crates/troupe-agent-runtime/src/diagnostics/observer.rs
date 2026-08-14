use std::any::Any;
use std::fmt;
use std::sync::Arc;

#[derive(Clone)]
pub struct AgentDiagnosticObserver {
    destination: Arc<dyn Any + Send + Sync>,
}

impl AgentDiagnosticObserver {
    pub fn from_destination<T>(destination: Arc<T>) -> Self
    where
        T: Any + Send + Sync,
    {
        Self { destination }
    }

    pub fn same_destination(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.destination, &other.destination)
    }
}

impl fmt::Debug for AgentDiagnosticObserver {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("AgentDiagnosticObserver")
            .finish_non_exhaustive()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AgentDiagnosticObserverInstallError {
    AlreadyInstalled,
    SessionOpeningStarted,
}

impl fmt::Display for AgentDiagnosticObserverInstallError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::AlreadyInstalled => "an agent diagnostic observer is already installed",
            Self::SessionOpeningStarted => {
                "the agent diagnostic observer must be installed before session opening"
            }
        })
    }
}

impl std::error::Error for AgentDiagnosticObserverInstallError {}
