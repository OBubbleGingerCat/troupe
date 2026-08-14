use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::sync::{Arc, Mutex, MutexGuard};

use pyo3::class::gc::{PyTraverseError, PyVisit};
use pyo3::create_exception;
use pyo3::exceptions::{PyRuntimeError, PyTypeError};
use pyo3::prelude::*;
use pyo3::types::{PyDict, PyString, PyTuple, PyType};

use crate::diagnostic_runtime::effect_producer::{self, EffectHook};

create_exception!(troupe, EffectContextError, PyRuntimeError);

const EFFECT_DIRECT_ERROR: &str = "Effect instances can only be created by Actor.make_effect()";
const EFFECT_RESULT_ERROR: &str = "effect_type did not construct the requested Effect instance";

#[derive(Debug)]
pub(crate) struct EffectIdentity;

pub(crate) struct EffectConstruction {
    effect_type: Py<PyType>,
    id: Py<PyString>,
    owner: Py<PyString>,
    identity: Arc<EffectIdentity>,
    pid: u32,
    consumed: Cell<bool>,
}

impl EffectConstruction {
    fn new(
        effect_type: Py<PyType>,
        id: Py<PyString>,
        owner: Py<PyString>,
        identity: Arc<EffectIdentity>,
    ) -> Self {
        Self {
            effect_type,
            id,
            owner,
            identity,
            pid: std::process::id(),
            consumed: Cell::new(false),
        }
    }

    fn was_consumed(&self) -> bool {
        self.consumed.get()
    }

    fn matches(&self, effect: &Bound<'_, Effect>) -> bool {
        Arc::ptr_eq(&self.identity, &effect.borrow().identity)
    }

    #[allow(dead_code)]
    pub(crate) fn effect_type(&self, py: Python<'_>) -> Py<PyType> {
        self.effect_type.clone_ref(py)
    }

    #[allow(dead_code)]
    pub(crate) fn id(&self, py: Python<'_>) -> Py<PyString> {
        self.id.clone_ref(py)
    }

    #[allow(dead_code)]
    pub(crate) fn owner(&self, py: Python<'_>) -> Py<PyString> {
        self.owner.clone_ref(py)
    }

    #[allow(dead_code)]
    pub(crate) fn identity(&self) -> &Arc<EffectIdentity> {
        &self.identity
    }
}

thread_local! {
    static EFFECT_PERMITS: RefCell<Vec<Rc<EffectConstruction>>> = const { RefCell::new(Vec::new()) };
}

struct EffectPermitGuard {
    construction: Rc<EffectConstruction>,
}

impl Drop for EffectPermitGuard {
    fn drop(&mut self) {
        EFFECT_PERMITS.with(|permits| {
            let popped = permits
                .borrow_mut()
                .pop()
                .expect("Effect permit stack must contain its active guard");
            assert!(
                Rc::ptr_eq(&popped, &self.construction),
                "Effect permit guards must be dropped in LIFO order"
            );
        });
    }
}

fn enter_effect_permit(
    effect_type: &Bound<'_, PyType>,
    id: &Bound<'_, PyString>,
    owner: &Bound<'_, PyString>,
) -> (Rc<EffectConstruction>, EffectPermitGuard) {
    let construction = Rc::new(EffectConstruction::new(
        effect_type.clone().unbind(),
        id.clone().unbind(),
        owner.clone().unbind(),
        Arc::new(EffectIdentity),
    ));
    EFFECT_PERMITS.with(|permits| permits.borrow_mut().push(Rc::clone(&construction)));
    let guard = EffectPermitGuard {
        construction: Rc::clone(&construction),
    };
    (construction, guard)
}

fn consume_effect_permit(cls: &Bound<'_, PyType>) -> PyResult<Effect> {
    let construction = EFFECT_PERMITS.with(|permits| permits.borrow().last().cloned());
    let Some(construction) = construction else {
        return Err(PyTypeError::new_err(EFFECT_DIRECT_ERROR));
    };
    if construction.pid != std::process::id()
        || !construction.effect_type.bind(cls.py()).is(cls)
        || construction.consumed.replace(true)
    {
        return Err(PyTypeError::new_err(EFFECT_DIRECT_ERROR));
    }

    let effect = Effect {
        id: Mutex::new(Some(construction.id.clone_ref(cls.py()))),
        owner: Mutex::new(Some(construction.owner.clone_ref(cls.py()))),
        identity: Arc::clone(&construction.identity),
    };
    effect_producer::observe(&effect, EffectHook::Created);
    Ok(effect)
}

pub(crate) fn construct_effect(
    effect_type: &Bound<'_, PyType>,
    args: &Bound<'_, PyTuple>,
    kwargs: &Bound<'_, PyDict>,
    id: Py<PyString>,
    owner: Py<PyString>,
) -> PyResult<Py<PyAny>> {
    let py = effect_type.py();
    let (construction, guard) = enter_effect_permit(effect_type, id.bind(py), owner.bind(py));
    effect_producer::construction_started(&construction);
    let result = (|| {
        let result = effect_type.call(args, Some(kwargs));
        drop(guard);
        let result = result?;
        if !construction.was_consumed() {
            return Err(PyTypeError::new_err(EFFECT_RESULT_ERROR));
        }
        let effect = result
            .cast::<Effect>()
            .map_err(|_| PyTypeError::new_err(EFFECT_RESULT_ERROR))?;
        if !construction.matches(effect) {
            return Err(PyTypeError::new_err(EFFECT_RESULT_ERROR));
        }
        Ok(result.unbind())
    })();
    match &result {
        Ok(value) => {
            let effect = value
                .bind(py)
                .cast::<Effect>()
                .expect("validated effect construction result");
            effect_producer::construction_finished(&construction, Ok(effect));
        }
        Err(error) => {
            effect_producer::construction_finished(&construction, Err(error));
        }
    }
    result
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[pyclass(name = "Effect", module = "troupe", subclass, dict)]
pub struct Effect {
    id: Mutex<Option<Py<PyString>>>,
    owner: Mutex<Option<Py<PyString>>>,
    identity: Arc<EffectIdentity>,
}

impl Effect {
    #[allow(dead_code)]
    pub(crate) fn diagnostic_id(&self, py: Python<'_>) -> Option<Py<PyString>> {
        lock(&self.id).as_ref().map(|id| id.clone_ref(py))
    }

    #[allow(dead_code)]
    pub(crate) fn diagnostic_owner(&self, py: Python<'_>) -> Option<Py<PyString>> {
        lock(&self.owner).as_ref().map(|owner| owner.clone_ref(py))
    }

    #[allow(dead_code)]
    pub(crate) fn diagnostic_identity(&self) -> &Arc<EffectIdentity> {
        &self.identity
    }
}

#[pymethods]
impl Effect {
    #[new]
    #[classmethod]
    #[pyo3(signature = (*args, **kwargs))]
    fn new(
        cls: &Bound<'_, PyType>,
        args: &Bound<'_, PyTuple>,
        kwargs: Option<&Bound<'_, PyDict>>,
    ) -> PyResult<Self> {
        let _ = (args, kwargs);
        consume_effect_permit(cls)
    }

    #[getter]
    fn id(&self, py: Python<'_>) -> PyResult<Py<PyString>> {
        lock(&self.id)
            .as_ref()
            .map(|value| value.clone_ref(py))
            .ok_or_else(|| PyRuntimeError::new_err("Effect is no longer attached"))
    }

    #[getter]
    fn owner(&self, py: Python<'_>) -> PyResult<Py<PyString>> {
        lock(&self.owner)
            .as_ref()
            .map(|value| value.clone_ref(py))
            .ok_or_else(|| PyRuntimeError::new_err("Effect is no longer attached"))
    }

    fn __traverse__(&self, visit: PyVisit<'_>) -> Result<(), PyTraverseError> {
        visit.call(&*lock(&self.id))?;
        visit.call(&*lock(&self.owner))
    }

    fn __clear__(&self) {
        let id = lock(&self.id).take();
        let owner = lock(&self.owner).take();
        if id.is_some() || owner.is_some() {
            effect_producer::cleared(self, id.as_ref(), owner.as_ref());
        }
        drop((id, owner));
    }
}

#[cfg(test)]
mod tests {
    use std::thread;

    use pyo3::prelude::*;
    use pyo3::types::{PyString, PyType};

    use super::{EFFECT_DIRECT_ERROR, consume_effect_permit, enter_effect_permit, lock};

    fn type_named<'py>(py: Python<'py>, name: &str) -> Bound<'py, PyType> {
        py.import("builtins")
            .expect("builtins must import")
            .getattr(name)
            .expect("builtin type must exist")
            .cast_into::<PyType>()
            .expect("builtin object must be a type")
    }

    #[test]
    fn effect_permits_are_thread_local_lifo_exact_and_single_use() {
        let _python_test_guard = crate::initialize_python_for_test();
        Python::attach(|py| {
            let outer_type = type_named(py, "int");
            let inner_type = type_named(py, "str");
            let outer_id = PyString::new(py, "scene-cue0-effect0");
            let inner_id = PyString::new(py, "scene-cue0-effect1");
            let owner = PyString::new(py, "actor");
            let (_, outer_guard) = enter_effect_permit(&outer_type, &outer_id, &owner);
            let (_, inner_guard) = enter_effect_permit(&inner_type, &inner_id, &owner);

            let mismatch = match consume_effect_permit(&outer_type) {
                Ok(_) => panic!("the top permit must require its exact class"),
                Err(error) => error,
            };
            assert_eq!(
                mismatch.to_string(),
                format!("TypeError: {EFFECT_DIRECT_ERROR}")
            );
            consume_effect_permit(&inner_type).expect("the matching inner permit must be consumed");
            assert!(consume_effect_permit(&inner_type).is_err());
            drop(inner_guard);

            let foreign_type = outer_type.clone().unbind();
            let foreign_error = py.detach(move || {
                thread::spawn(move || {
                    Python::attach(|thread_py| {
                        match consume_effect_permit(foreign_type.bind(thread_py)) {
                            Ok(_) => panic!("another thread must not observe the permit"),
                            Err(error) => error.to_string(),
                        }
                    })
                })
                .join()
                .expect("permit probe thread must join")
            });
            assert_eq!(foreign_error, format!("TypeError: {EFFECT_DIRECT_ERROR}"));
            let outer = consume_effect_permit(&outer_type)
                .expect("the owner thread must retain its permit");
            let id = lock(&outer.id)
                .as_ref()
                .expect("constructed Effect must retain an id")
                .clone_ref(py);
            let owner = lock(&outer.owner)
                .as_ref()
                .expect("constructed Effect must retain an owner")
                .clone_ref(py);
            assert_eq!(id.bind(py).to_str()?, "scene-cue0-effect0");
            assert_eq!(owner.bind(py).to_str()?, "actor");
            assert!(consume_effect_permit(&outer_type).is_err());
            drop(outer_guard);
            assert!(consume_effect_permit(&outer_type).is_err());
            Ok::<_, PyErr>(())
        })
        .expect("Effect permit isolation test must complete");
    }
}
