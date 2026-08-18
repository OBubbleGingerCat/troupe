use std::path::{Path, PathBuf};

use pyo3::exceptions::PyImportError;
use pyo3::prelude::*;
use pyo3::types::{
    PyAny, PyAnyMethods, PyDict, PyDictMethods, PyList, PyModule, PyModuleMethods, PyString,
    PyType, PyTypeMethods,
};

use crate::orchestration::production::Production;

use super::path::resolved_path;
use super::{LoadFailure, Reason, ResolvedProductionPath, fail, finish_failure};

struct ModuleSnapshot {
    key: Py<PyString>,
    module: Py<PyAny>,
    dictionary: Py<PyDict>,
    saved_dictionary: Py<PyDict>,
}

pub(super) struct ImportTransaction {
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

pub(crate) struct ResolvedProductionClass {
    pub(super) path: ResolvedProductionPath,
    pub(super) production_type: Py<PyType>,
    pub(super) transaction: ImportTransaction,
}

impl ResolvedProductionClass {
    // B08 consumes this seam before calling construct_production.
    #[allow(dead_code)]
    pub(crate) fn inspect_static_attribute(
        &self,
        py: Python<'_>,
        name: &str,
    ) -> PyResult<Option<Py<PyAny>>> {
        inspect_static_class_attribute(py, self.production_type.bind(py), name)
    }

    pub(super) fn package_dir<'py>(&self, py: Python<'py>) -> &Bound<'py, PyAny> {
        self.path.production_root(py)
    }

    pub(super) fn production_type<'py>(&self, py: Python<'py>) -> &Bound<'py, PyType> {
        self.production_type.bind(py)
    }

    pub(crate) fn rollback(self, py: Python<'_>) -> PyResult<()> {
        self.transaction.rollback(py)
    }
}

fn inspect_static_class_attribute(
    py: Python<'_>,
    production_type: &Bound<'_, PyType>,
    name: &str,
) -> PyResult<Option<Py<PyAny>>> {
    let sentinel = PyDict::new(py);
    let value = py.import("inspect")?.getattr("getattr_static")?.call1((
        production_type,
        name,
        &sentinel,
    ))?;
    if value.is(sentinel.as_any()) {
        Ok(None)
    } else {
        Ok(Some(value.unbind()))
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

fn module_dictionary<'py>(module: &Bound<'py, PyAny>) -> PyResult<Bound<'py, PyDict>> {
    Ok(module.cast::<PyModule>()?.dict())
}

fn path_is_inside(path: &Path, package_dir: &Path) -> bool {
    path == package_dir || path.starts_with(package_dir)
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

fn resolve_in_transaction(
    py: Python<'_>,
    path: &ResolvedProductionPath,
    preloaded_production: Option<&Py<PyAny>>,
) -> Result<Py<PyType>, LoadFailure> {
    let modules = py
        .import("sys")
        .and_then(|sys| sys.getattr("modules"))
        .and_then(|modules| modules.cast_into::<PyDict>().map_err(Into::into))
        .map_err(|error| LoadFailure::from_error(py, Reason::ImportFailed, error))?;
    let package = import_package(
        py,
        &modules,
        &path.root,
        path.production_root(py),
        path.init_path(py),
    )
    .map_err(|error| LoadFailure::from_error(py, Reason::ImportFailed, error))?;
    let production_name = format!("{}.production", path.root);
    let production_module = import_production(
        py,
        &modules,
        &package,
        &production_name,
        path.production_path(py),
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

    Ok(production_type.unbind())
}

pub(crate) fn resolve_production_class(
    py: Python<'_>,
    path: ResolvedProductionPath,
) -> PyResult<ResolvedProductionClass> {
    let package_path = path.production_root(py).extract::<PathBuf>()?;
    let modules = py
        .import("sys")?
        .getattr("modules")?
        .cast_into::<PyDict>()?;
    if has_package_conflict(py, &modules, &path.root, &package_path)? {
        return fail(py, path.production_root(py), Reason::PackageNameConflict);
    }

    let production_name = format!("{}.production", path.root);
    let expected_production_path = path
        .production_path(py)
        .call_method0("resolve")?
        .extract::<PathBuf>()?;
    let preloaded_production = match modules.get_item(&production_name)? {
        Some(module) if module_origin_matches(py, &module, &expected_production_path)? => {
            Some(module.unbind())
        }
        _ => None,
    };
    let transaction = ImportTransaction::snapshot(&path.root, &modules)?;
    let production_type = match resolve_in_transaction(py, &path, preloaded_production.as_ref()) {
        Ok(production_type) => production_type,
        Err(failure) => {
            transaction.rollback(py)?;
            return finish_failure(py, path.production_root(py), failure);
        }
    };

    Ok(ResolvedProductionClass {
        path,
        production_type,
        transaction,
    })
}

#[cfg(test)]
mod tests {
    use pyo3::types::{PyDict, PyDictMethods, PyList};

    use super::*;

    #[test]
    fn static_class_inspection_does_not_invoke_descriptor_or_constructor() {
        let _python_test_guard = crate::initialize_python_for_test();
        Python::attach(|py| {
            let globals = PyDict::new(py);
            globals.set_item("BaseProduction", py.get_type::<Production>())?;
            py.run(
                c"state = {'descriptor_calls': 0, 'constructor_calls': 0}\n\nclass Probe:\n    def __get__(self, instance, owner):\n        state['descriptor_calls'] += 1\n        return ('dynamic',)\n\nprobe = Probe()\n\nclass Candidate(BaseProduction):\n    ignored_extension = probe\n\n    def __init__(self, args):\n        state['constructor_calls'] += 1\n",
                Some(&globals),
                None,
            )?;
            let candidate = globals
                .get_item("Candidate")?
                .expect("candidate class must exist")
                .cast_into::<PyType>()?;
            let probe = globals.get_item("probe")?.expect("probe must exist");
            let state = globals
                .get_item("state")?
                .expect("state must exist")
                .cast_into::<PyDict>()?;

            let inspected = inspect_static_class_attribute(py, &candidate, "ignored_extension")?
                .expect("ignored_extension must exist");
            assert!(inspected.bind(py).is(&probe));
            assert_eq!(
                state
                    .get_item("descriptor_calls")?
                    .expect("descriptor count must exist")
                    .extract::<usize>()?,
                0
            );
            assert_eq!(
                state
                    .get_item("constructor_calls")?
                    .expect("constructor count must exist")
                    .extract::<usize>()?,
                0
            );

            candidate.call1((PyList::empty(py),))?;
            assert_eq!(
                state
                    .get_item("constructor_calls")?
                    .expect("constructor count must exist")
                    .extract::<usize>()?,
                1
            );
            Ok::<_, PyErr>(())
        })
        .expect("static inspection must stay before construction");
    }
}
