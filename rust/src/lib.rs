use pyo3::pymodule;

mod cli;
mod diagnostics;
mod failure;
mod invocation;
mod loader;
mod production;
mod python_task;
mod runtime;
mod signals;

#[pymodule(gil_used = true)]
mod _runtime {
    #[pymodule_export]
    use crate::cli::main;
    #[pymodule_export]
    use crate::diagnostics::format_failure_for_test;
    #[pymodule_export]
    use crate::failure::{PhaseFailure, ProductionFailed};
    #[pymodule_export]
    use crate::invocation::parse_invocation;
    #[pymodule_export]
    use crate::loader::{ProductionLoadError, load_production};
    #[pymodule_export]
    use crate::production::Production;
    #[pymodule_export]
    use crate::runtime::Runtime;
}
