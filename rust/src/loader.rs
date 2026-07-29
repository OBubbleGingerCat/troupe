use std::path::{Path, PathBuf};

use pyo3::create_exception;
use pyo3::exceptions::{PyException, PyImportError, PySystemExit};
use pyo3::prelude::*;
use pyo3::types::{
    PyAnyMethods, PyDict, PyDictMethods, PyList, PyModule, PyModuleMethods, PyString,
    PyStringMethods, PyType, PyTypeMethods,
};

use crate::production::Production;

create_exception!(troupe._runtime, ProductionLoadError, PyException);

#[derive(Clone, Copy)]
enum Reason {
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

enum LoadFailure {
    Reason(Reason),
    Caused(Reason, PyErr),
    Propagate(PyErr),
}

impl LoadFailure {
    fn from_error(py: Python<'_>, reason: Reason, error: PyErr) -> Self {
        if error.is_instance_of::<PySystemExit>(py) {
            Self::Propagate(error)
        } else {
            Self::Caused(reason, error)
        }
    }
}

struct ModuleSnapshot {
    key: Py<PyString>,
    module: Py<PyAny>,
    dictionary: Py<PyDict>,
    saved_dictionary: Py<PyDict>,
}

struct ImportTransaction {
    root: String,
    modules: Py<PyDict>,
    snapshots: Vec<ModuleSnapshot>,
}

impl ImportTransaction {
    fn snapshot(root: &str, modules: &Bound<'_, PyDict>) -> PyResult<Self> {
        let mut snapshots = Vec::new();
        for (key, module) in modules.iter() {
            let Ok(key) = key.cast_into::<PyString>() else {
                continue;
            };
            if !is_prefix_key(&key, root)? {
                continue;
            }
            let dictionary = module_dictionary(&module)?;
            let saved_dictionary = dictionary.copy()?.unbind();
            snapshots.push(ModuleSnapshot {
                key: key.unbind(),
                module: module.unbind(),
                dictionary: dictionary.unbind(),
                saved_dictionary,
            });
        }
        Ok(Self {
            root: root.to_owned(),
            modules: modules.clone().unbind(),
            snapshots,
        })
    }

    fn rollback(&self, py: Python<'_>) -> PyResult<()> {
        let modules = self.modules.bind(py);
        let mut current_keys = Vec::new();
        for (key, _) in modules.iter() {
            let Ok(key) = key.cast_into::<PyString>() else {
                continue;
            };
            if is_prefix_key(&key, &self.root)? {
                let depth = string_depth(&key)?;
                current_keys.push((depth, key.unbind()));
            }
        }
        current_keys.sort_by(|(left, _), (right, _)| right.cmp(left));
        for (_, key) in current_keys {
            modules.del_item(key.bind(py))?;
        }

        for snapshot in &self.snapshots {
            modules.set_item(snapshot.key.bind(py), snapshot.module.bind(py))?;
        }
        for snapshot in &self.snapshots {
            let dictionary = snapshot.dictionary.bind(py);
            dictionary.clear();
            dictionary.update(snapshot.saved_dictionary.bind(py).as_mapping())?;
        }
        Ok(())
    }
}

fn is_prefix_key(key: &Bound<'_, PyString>, root: &str) -> PyResult<bool> {
    if key == root {
        return Ok(true);
    }
    key.py()
        .get_type::<PyString>()
        .getattr("startswith")?
        .call1((key, format!("{root}.")))?
        .extract()
}

fn string_depth(key: &Bound<'_, PyString>) -> PyResult<usize> {
    key.py()
        .get_type::<PyString>()
        .getattr("count")?
        .call1((key, "."))?
        .extract()
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

fn fail<T>(py: Python<'_>, package_dir: &Bound<'_, PyAny>, reason: Reason) -> PyResult<T> {
    Err(production_load_error(py, package_dir, reason)?)
}

fn resolved_path<'py>(py: Python<'py>, value: &Bound<'py, PyAny>) -> PyResult<Bound<'py, PyAny>> {
    py.import("pathlib")?
        .getattr("Path")?
        .call1((value,))?
        .call_method0("resolve")
}

fn path_is_inside(path: &Path, package_dir: &Path) -> bool {
    path == package_dir || path.starts_with(package_dir)
}

fn module_dictionary<'py>(module: &Bound<'py, PyAny>) -> PyResult<Bound<'py, PyDict>> {
    Ok(module.cast::<PyModule>()?.dict())
}

fn module_belongs_to_package(
    py: Python<'_>,
    module: &Bound<'_, PyAny>,
    package_dir: &Path,
) -> PyResult<bool> {
    if module_dictionary(module).is_err() {
        return Ok(false);
    }

    let Ok(spec) = module.getattr("__spec__") else {
        return Ok(false);
    };
    if spec.is_none() {
        return Ok(false);
    }
    let Ok(origin) = spec.getattr("origin") else {
        return Ok(false);
    };

    if origin.is_none() {
        let Ok(locations) = spec.getattr("submodule_search_locations") else {
            return Ok(false);
        };
        if locations.is_none() {
            return Ok(false);
        }
        let Ok(iterator) = locations.try_iter() else {
            return Ok(false);
        };
        let mut found = false;
        for location in iterator {
            let Ok(location) = location else {
                return Ok(false);
            };
            let Ok(location) = resolved_path(py, &location) else {
                return Ok(false);
            };
            let Ok(location) = location.extract::<PathBuf>() else {
                return Ok(false);
            };
            if !path_is_inside(&location, package_dir) {
                return Ok(false);
            }
            found = true;
        }
        return Ok(found);
    }

    if origin.eq("built-in")? || origin.eq("frozen")? {
        return Ok(false);
    }
    let Ok(origin) = resolved_path(py, &origin) else {
        return Ok(false);
    };
    let Ok(origin) = origin.extract::<PathBuf>() else {
        return Ok(false);
    };
    Ok(path_is_inside(&origin, package_dir))
}

fn has_package_conflict(
    py: Python<'_>,
    modules: &Bound<'_, PyDict>,
    root: &str,
    package_dir: &Path,
) -> PyResult<bool> {
    for (key, module) in modules.iter() {
        let Ok(key) = key.cast_into::<PyString>() else {
            continue;
        };
        if is_prefix_key(&key, root)? && !module_belongs_to_package(py, &module, package_dir)? {
            return Ok(true);
        }
    }
    Ok(false)
}

fn import_package<'py>(
    py: Python<'py>,
    modules: &Bound<'py, PyDict>,
    root: &str,
    package_dir: &Bound<'py, PyAny>,
    init_path: &Bound<'py, PyAny>,
) -> PyResult<Bound<'py, PyAny>> {
    if let Some(package) = modules.get_item(root)? {
        return Ok(package);
    }

    let util = py.import("importlib.util")?;
    let kwargs = PyDict::new(py);
    let search_location = py.import("os")?.call_method1("fspath", (package_dir,))?;
    kwargs.set_item(
        "submodule_search_locations",
        PyList::new(py, [search_location])?,
    )?;
    let spec = util
        .getattr("spec_from_file_location")?
        .call((root, init_path), Some(&kwargs))?;
    let package = util.getattr("module_from_spec")?.call1((&spec,))?;
    modules.set_item(root, &package)?;
    spec.getattr("loader")?
        .call_method1("exec_module", (&package,))?;
    canonical_module(modules, root)
}

fn canonical_module<'py>(modules: &Bound<'py, PyDict>, name: &str) -> PyResult<Bound<'py, PyAny>> {
    modules.get_item(name)?.ok_or_else(|| {
        PyImportError::new_err(format!("module {name} removed itself from sys.modules"))
    })
}

fn module_origin_matches(
    py: Python<'_>,
    module: &Bound<'_, PyAny>,
    expected: &Path,
) -> PyResult<bool> {
    let Ok(spec) = module.getattr("__spec__") else {
        return Ok(false);
    };
    if spec.is_none() {
        return Ok(false);
    }
    let Ok(origin) = spec.getattr("origin") else {
        return Ok(false);
    };
    if origin.is_none() || origin.eq("built-in")? || origin.eq("frozen")? {
        return Ok(false);
    }
    let Ok(origin) = resolved_path(py, &origin) else {
        return Ok(false);
    };
    let Ok(origin) = origin.extract::<PathBuf>() else {
        return Ok(false);
    };
    Ok(origin == expected)
}

fn import_production<'py>(
    py: Python<'py>,
    modules: &Bound<'py, PyDict>,
    package: &Bound<'py, PyAny>,
    name: &str,
    production_path: &Bound<'py, PyAny>,
    preloaded: Option<&Py<PyAny>>,
) -> PyResult<Bound<'py, PyAny>> {
    let expected_path = production_path
        .call_method0("resolve")?
        .extract::<PathBuf>()?;
    let module = if let Some(preloaded) = preloaded {
        let preloaded = preloaded.bind(py);
        modules.set_item(name, preloaded)?;
        preloaded.clone()
    } else if let Some(current) = modules.get_item(name)? {
        if module_origin_matches(py, &current, &expected_path)? {
            current
        } else {
            execute_production(py, modules, name, production_path)?
        }
    } else {
        execute_production(py, modules, name, production_path)?
    };

    module_dictionary(package)?.set_item("production", &module)?;
    Ok(module)
}

fn execute_production<'py>(
    py: Python<'py>,
    modules: &Bound<'py, PyDict>,
    name: &str,
    production_path: &Bound<'py, PyAny>,
) -> PyResult<Bound<'py, PyAny>> {
    let util = py.import("importlib.util")?;
    let spec = util
        .getattr("spec_from_file_location")?
        .call1((name, production_path))?;
    let module = util.getattr("module_from_spec")?.call1((&spec,))?;
    modules.set_item(name, &module)?;
    spec.getattr("loader")?
        .call_method1("exec_module", (&module,))?;
    canonical_module(modules, name)
}

fn load_in_transaction(
    py: Python<'_>,
    root: &str,
    package_dir: &Bound<'_, PyAny>,
    init_path: &Bound<'_, PyAny>,
    production_path: &Bound<'_, PyAny>,
    preloaded_production: Option<&Py<PyAny>>,
    args: &Bound<'_, PyList>,
) -> Result<Py<PyAny>, LoadFailure> {
    let modules = py
        .import("sys")
        .and_then(|sys| sys.getattr("modules"))
        .and_then(|modules| modules.cast_into::<PyDict>().map_err(Into::into))
        .map_err(|error| LoadFailure::from_error(py, Reason::ImportFailed, error))?;
    let package = import_package(py, &modules, root, package_dir, init_path)
        .map_err(|error| LoadFailure::from_error(py, Reason::ImportFailed, error))?;
    let production_name = format!("{root}.production");
    let production_module = import_production(
        py,
        &modules,
        &package,
        &production_name,
        production_path,
        preloaded_production,
    )
    .map_err(|error| LoadFailure::from_error(py, Reason::ImportFailed, error))?;
    let dictionary = module_dictionary(&production_module)
        .map_err(|error| LoadFailure::from_error(py, Reason::ImportFailed, error))?;
    let Some(symbol) = dictionary
        .get_item("Production")
        .map_err(|error| LoadFailure::from_error(py, Reason::ImportFailed, error))?
    else {
        return Err(LoadFailure::Reason(Reason::MissingSymbol));
    };
    let Ok(production_type) = symbol.cast_into::<PyType>() else {
        return Err(LoadFailure::Reason(Reason::SymbolNotClass));
    };
    let base = py.get_type::<Production>();
    if production_type.as_any().is(base.as_any()) {
        return Err(LoadFailure::Reason(Reason::SymbolIsBase));
    }
    let is_subclass = production_type
        .is_subclass(base.as_any())
        .map_err(|error| LoadFailure::from_error(py, Reason::ImportFailed, error))?;
    if !is_subclass {
        return Err(LoadFailure::Reason(Reason::SymbolNotSubclass));
    }

    production_type
        .call1((args,))
        .map(Bound::unbind)
        .map_err(|error| LoadFailure::from_error(py, Reason::ConstructionFailed, error))
}

#[pyfunction(name = "_load_production")]
pub fn load_production(
    py: Python<'_>,
    package_dir: &Bound<'_, PyString>,
    args: &Bound<'_, PyList>,
) -> PyResult<Py<PyAny>> {
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

    let package_path = resolved.extract::<PathBuf>()?;
    let modules = py
        .import("sys")?
        .getattr("modules")?
        .cast_into::<PyDict>()?;
    if has_package_conflict(py, &modules, &root, &package_path)? {
        return fail(py, &resolved, Reason::PackageNameConflict);
    }

    let production_name = format!("{root}.production");
    let expected_production_path = production_path
        .call_method0("resolve")?
        .extract::<PathBuf>()?;
    let preloaded_production = match modules.get_item(&production_name)? {
        Some(module) if module_origin_matches(py, &module, &expected_production_path)? => {
            Some(module.unbind())
        }
        _ => None,
    };
    let transaction = ImportTransaction::snapshot(&root, &modules)?;
    match load_in_transaction(
        py,
        &root,
        &resolved,
        &init_path,
        &production_path,
        preloaded_production.as_ref(),
        args,
    ) {
        Ok(production) => Ok(production),
        Err(failure) => {
            transaction.rollback(py)?;
            match failure {
                LoadFailure::Reason(reason) => fail(py, &resolved, reason),
                LoadFailure::Caused(reason, cause) => {
                    let error = production_load_error(py, &resolved, reason)?;
                    error.set_cause(py, Some(cause));
                    Err(error)
                }
                LoadFailure::Propagate(error) => Err(error),
            }
        }
    }
}
