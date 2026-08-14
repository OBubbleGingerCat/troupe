mod class;
mod construct;
mod path;

use pyo3::create_exception;
use pyo3::exceptions::{PyException, PySystemExit};
use pyo3::prelude::*;
use pyo3::types::{PyAny, PyList, PyString};

pub(crate) use class::{ResolvedProductionClass, resolve_production_class};
pub(crate) use construct::construct_production;
pub(crate) use path::{ResolvedProductionPath, resolve_production_path};

create_exception!(troupe._runtime, ProductionLoadError, PyException);

#[derive(Clone, Copy)]
pub(super) enum Reason {
    PathNotDirectory,
    InvalidPackageName,
    MissingInit,
    MissingProduction,
    PackageNameConflict,
    ImportFailed,
    MissingSymbol,
    SymbolNotClass,
    SymbolIsBase,
    SymbolNotSubclass,
    ConstructionFailed,
}

impl Reason {
    fn as_str(self) -> &'static str {
        match self {
            Self::PathNotDirectory => "path-not-directory",
            Self::InvalidPackageName => "invalid-package-name",
            Self::MissingInit => "missing-init",
            Self::MissingProduction => "missing-production",
            Self::PackageNameConflict => "package-name-conflict",
            Self::ImportFailed => "import-failed",
            Self::MissingSymbol => "missing-symbol",
            Self::SymbolNotClass => "symbol-not-class",
            Self::SymbolIsBase => "symbol-is-base",
            Self::SymbolNotSubclass => "symbol-not-subclass",
            Self::ConstructionFailed => "construction-failed",
        }
    }
}

pub(super) enum LoadFailure {
    Reason(Reason),
    Caused(Reason, PyErr),
    Propagate(PyErr),
}

impl LoadFailure {
    pub(super) fn from_error(py: Python<'_>, reason: Reason, error: PyErr) -> Self {
        if error.is_instance_of::<PySystemExit>(py) {
            Self::Propagate(error)
        } else {
            Self::Caused(reason, error)
        }
    }
}

fn production_load_error(
    py: Python<'_>,
    package_dir: &Bound<'_, PyAny>,
    reason: Reason,
) -> PyResult<PyErr> {
    let reason = reason.as_str();
    let message = PyString::new(py, "cannot load Production from {}: {}")
        .call_method1("format", (package_dir, reason))?;
    let error = PyErr::from_value(py.get_type::<ProductionLoadError>().call1((message,))?);
    error.value(py).setattr("package_dir", package_dir)?;
    error.value(py).setattr("reason", reason)?;
    Ok(error)
}

pub(super) fn fail<T>(
    py: Python<'_>,
    package_dir: &Bound<'_, PyAny>,
    reason: Reason,
) -> PyResult<T> {
    Err(production_load_error(py, package_dir, reason)?)
}

pub(super) fn finish_failure<T>(
    py: Python<'_>,
    package_dir: &Bound<'_, PyAny>,
    failure: LoadFailure,
) -> PyResult<T> {
    match failure {
        LoadFailure::Reason(reason) => fail(py, package_dir, reason),
        LoadFailure::Caused(reason, cause) => {
            let error = production_load_error(py, package_dir, reason)?;
            error.set_cause(py, Some(cause));
            Err(error)
        }
        LoadFailure::Propagate(error) => Err(error),
    }
}

#[pyfunction(name = "_load_production")]
pub fn load_production(
    py: Python<'_>,
    package_dir: &Bound<'_, PyString>,
    args: &Bound<'_, PyList>,
) -> PyResult<Py<PyAny>> {
    let path = resolve_production_path(py, package_dir)?;
    let production_class = resolve_production_class(py, path)?;
    construct_production(py, production_class, args)
}
