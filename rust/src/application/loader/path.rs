use pyo3::prelude::*;
use pyo3::types::{PyAny, PyAnyMethods, PyString, PyStringMethods};

use super::{Reason, fail};

pub(crate) struct ResolvedProductionPath {
    pub(super) package_dir: Py<PyAny>,
    pub(super) root: String,
    pub(super) init_path: Py<PyAny>,
    pub(super) production_path: Py<PyAny>,
}

impl ResolvedProductionPath {
    pub(crate) fn production_root<'py>(&self, py: Python<'py>) -> &Bound<'py, PyAny> {
        self.package_dir.bind(py)
    }

    pub(super) fn init_path<'py>(&self, py: Python<'py>) -> &Bound<'py, PyAny> {
        self.init_path.bind(py)
    }

    pub(super) fn production_path<'py>(&self, py: Python<'py>) -> &Bound<'py, PyAny> {
        self.production_path.bind(py)
    }
}

pub(super) fn resolved_path<'py>(
    py: Python<'py>,
    value: &Bound<'py, PyAny>,
) -> PyResult<Bound<'py, PyAny>> {
    py.import("pathlib")?
        .getattr("Path")?
        .call1((value,))?
        .call_method0("resolve")
}

pub(crate) fn resolve_production_path(
    py: Python<'_>,
    package_dir: &Bound<'_, PyString>,
) -> PyResult<ResolvedProductionPath> {
    let resolved = resolved_path(py, package_dir.as_any())?;
    if !resolved.call_method0("is_dir")?.is_truthy()? {
        return fail(py, &resolved, Reason::PathNotDirectory);
    }

    let basename = resolved.getattr("name")?.cast_into::<PyString>()?;
    let is_identifier = basename.call_method0("isidentifier")?.is_truthy()?;
    let is_keyword = py
        .import("keyword")?
        .call_method1("iskeyword", (&basename,))?
        .is_truthy()?;
    let normalized = py
        .import("unicodedata")?
        .call_method1("normalize", ("NFKC", &basename))?;
    if !is_identifier || is_keyword || !normalized.eq(&basename)? {
        return fail(py, &resolved, Reason::InvalidPackageName);
    }
    let root = basename.to_str()?.to_owned();

    let init_path = resolved.call_method1("joinpath", ("__init__.py",))?;
    if !init_path.call_method0("is_file")?.is_truthy()? {
        return fail(py, &resolved, Reason::MissingInit);
    }
    let production_path = resolved.call_method1("joinpath", ("production.py",))?;
    if !production_path.call_method0("is_file")?.is_truthy()? {
        return fail(py, &resolved, Reason::MissingProduction);
    }

    Ok(ResolvedProductionPath {
        package_dir: resolved.unbind(),
        root,
        init_path: init_path.unbind(),
        production_path: production_path.unbind(),
    })
}
