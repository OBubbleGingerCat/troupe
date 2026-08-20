from __future__ import annotations

import importlib
import importlib.machinery
import importlib.util
import os
import pickle
import shutil
import sys
import types
import uuid
from collections.abc import Iterator
from pathlib import Path
from types import ModuleType
from typing import Any

import pytest


ROOT = Path(__file__).resolve().parents[2]
FIXTURES = ROOT / "tests" / "fixtures" / "productions"


class ModuleSandbox:
    def __init__(self) -> None:
        self.roots: set[str] = set()
        self.extra_names: set[str] = set()

    def track_root(self, root: str) -> str:
        self.roots.add(root)
        return root

    def install(self, name: str, module: ModuleType) -> ModuleType:
        self.extra_names.add(name)
        sys.modules[name] = module
        return module

    def state(self, **values: object) -> ModuleType:
        name = f"_troupe_loader_state_{uuid.uuid4().hex}"
        module = ModuleType(name)
        vars(module).update(values)
        return self.install(name, module)

    def cleanup(self) -> None:
        for root in self.roots:
            prefix = f"{root}."
            names = [
                name for name in sys.modules if name == root or str.startswith(name, prefix)
            ]
            for name in sorted(
                names,
                key=lambda value: str.count(value, "."),
                reverse=True,
            ):
                sys.modules.pop(name, None)
        for name in self.extra_names:
            sys.modules.pop(name, None)


@pytest.fixture
def modules() -> Iterator[ModuleSandbox]:
    sandbox = ModuleSandbox()
    try:
        yield sandbox
    finally:
        sandbox.cleanup()


def _runtime() -> ModuleType:
    return importlib.import_module("troupe._runtime")


def _load(package_dir: Path, args: list[str]) -> object:
    return _runtime()._load_production(str(package_dir), args)


def _prefix_modules(root: str) -> dict[str, ModuleType]:
    prefix = f"{root}."
    return {
        name: module
        for name, module in sys.modules.items()
        if name == root or str.startswith(name, prefix)
    }


def _assert_load_error(
    package_dir: Path,
    reason: str,
    *,
    args: list[str] | None = None,
    cause: BaseException | None = None,
) -> BaseException:
    runtime = _runtime()
    resolved = package_dir.resolve()

    with pytest.raises(runtime.ProductionLoadError) as caught:
        runtime._load_production(str(package_dir), [] if args is None else args)

    error = caught.value
    assert type(error) is runtime.ProductionLoadError
    assert runtime.ProductionLoadError.__module__ == "troupe._runtime"
    assert isinstance(error.package_dir, Path)
    assert error.package_dir == resolved
    assert error.reason == reason
    assert str(error) == f"cannot load Production from {resolved}: {reason}"
    assert error.__cause__ is cause
    return error


def _write_package(
    parent: Path,
    name: str,
    *,
    init: str | None = "",
    production: str | None = None,
) -> Path:
    package = parent / name
    package.mkdir(parents=True)
    if init is not None:
        (package / "__init__.py").write_text(init, encoding="utf-8")
    if production is not None:
        (package / "production.py").write_text(production, encoding="utf-8")
    return package


def _copy_fixture(tmp_path: Path, name: str, destination: str | None = None) -> Path:
    package = tmp_path / (name if destination is None else destination)
    shutil.copytree(FIXTURES / name, package)
    return package


def _valid_production_source() -> str:
    return (
        "from troupe import Production as BaseProduction\n"
        "class Production(BaseProduction):\n"
        "    def __init__(self, args):\n"
        "        self.received = args\n"
    )


def _traceback_frames(error: BaseException) -> list[tuple[str, str]]:
    frames: list[tuple[str, str]] = []
    traceback = error.__traceback__
    while traceback is not None:
        code = traceback.tb_frame.f_code
        frames.append((code.co_filename, code.co_name))
        traceback = traceback.tb_next
    return frames


def test_loader_source_exposes_path_class_and_construct_phases() -> None:
    application = ROOT / "rust" / "src" / "application"
    legacy = application / "loader.rs"
    loader = application / "loader"

    assert not legacy.exists()
    assert {path.name for path in loader.iterdir()} == {
        "class.rs",
        "construct.rs",
        "mod.rs",
        "path.rs",
    }

    module_source = (loader / "mod.rs").read_text(encoding="utf-8")
    for marker in (
        "prevalidate_production_root",
        "resolve_production_package",
        "resolve_production_class",
        "construct_production",
        "PrevalidatedProductionRoot",
        "ResolvedProductionClass",
        "ResolvedProductionPath",
    ):
        assert marker in module_source

    class_source = (loader / "class.rs").read_text(encoding="utf-8")
    path_source = (loader / "path.rs").read_text(encoding="utf-8")
    assert "pub(crate) fn package_candidate" in path_source
    assert "pub(crate) fn production_root" in path_source
    assert "pub(crate) fn inspect_static_attribute" in class_source
    assert "pub(crate) fn rollback" in class_source
    assert 'getattr("getattr_static")' in class_source

    invocation_source = (application / "invocation.rs").read_text(encoding="utf-8")
    assert "type ProductionInvocation<'py>" in invocation_source
    assert "Result<ParsedInvocation<'py>, InvocationError>" in invocation_source
    assert "PyResult<ProductionInvocation<'py>>" in invocation_source


def test_recording_package_loads_with_canonical_identity(
    modules: ModuleSandbox,
) -> None:
    root = modules.track_root("recording_production")
    package = FIXTURES / root
    repeated = "".join(["repeat", "-value"])
    surrogate = "\udcff"
    args = ["--item", repeated, "--item", repeated, surrogate]

    instance = _load(package, args)

    production_module = sys.modules[f"{root}.production"]
    package_module = sys.modules[root]
    assert type(instance.received) is list
    assert instance.received == args
    assert all(actual is expected for actual, expected in zip(instance.received, args))
    assert production_module.construction_count == 1
    assert instance.relative_config is instance.absolute_config
    assert instance.relative_config is sys.modules[f"{root}.config"]
    assert instance.config_value == "relative-and-absolute"
    assert instance.worker_value == "subpackage-loaded"
    assert instance.resource_value == "resource-loaded"
    assert type(instance).__module__ == f"{root}.production"
    assert pickle.loads(pickle.dumps(type(instance))) is type(instance)

    assert package_module.__name__ == root
    assert package_module.__package__ == root
    assert package_module.__spec__.name == root
    assert Path(package_module.__spec__.origin).resolve() == package / "__init__.py"
    assert production_module.__name__ == f"{root}.production"
    assert production_module.__package__ == root
    assert production_module.__spec__.name == f"{root}.production"
    assert Path(production_module.__spec__.origin).resolve() == package / "production.py"


def test_load_production_private_seam_requires_a_string(
    modules: ModuleSandbox,
) -> None:
    root = modules.track_root("same_name")

    with pytest.raises(TypeError):
        _runtime()._load_production(FIXTURES / root, [])


def test_loader_preserves_surrogateescape_in_parent_directory(
    tmp_path: Path, modules: ModuleSandbox
) -> None:
    parent = tmp_path / os.fsdecode(b"\xff")
    parent.mkdir()
    root = modules.track_root("byte_parent_package")
    package = _write_package(parent, root, production=_valid_production_source())

    instance = _load(package, ["value"])

    assert instance.received == ["value"]
    assert Path(sys.modules[f"{root}.production"].__spec__.origin) == (
        package / "production.py"
    )


def test_error_package_dir_is_resolved_from_a_relative_path(
    modules: ModuleSandbox,
) -> None:
    modules.track_root("relative_missing_package")
    relative = Path("tests") / ".." / "relative_missing_package"

    error = _assert_load_error(relative, "path-not-directory")

    assert error.package_dir == (ROOT / "relative_missing_package").resolve()


@pytest.mark.parametrize("kind", ["missing", "file"])
def test_path_not_directory_reason_precedes_name_validation(
    tmp_path: Path, modules: ModuleSandbox, kind: str
) -> None:
    modules.track_root("not-valid")
    path = tmp_path / "not-valid"
    if kind == "file":
        path.write_text("not a directory", encoding="utf-8")

    _assert_load_error(path, "path-not-directory")


@pytest.mark.parametrize("name", ["not-valid", "class", "\u212a"])
def test_invalid_package_names_fail_before_executing_code(
    tmp_path: Path, modules: ModuleSandbox, name: str
) -> None:
    modules.track_root(name)
    state = modules.state(executed=False)
    source = f"import {state.__name__}\n{state.__name__}.executed = True\n"
    package = _write_package(
        tmp_path,
        name,
        init=source,
        production=source + _valid_production_source(),
    )

    _assert_load_error(package, "invalid-package-name")

    assert state.executed is False
    assert _prefix_modules(name) == {}


@pytest.mark.parametrize(
    ("init", "production"),
    [(None, _valid_production_source()), ("", None)],
)
def test_invalid_package_name_precedes_entry_validation(
    tmp_path: Path,
    modules: ModuleSandbox,
    init: str | None,
    production: str | None,
) -> None:
    root = modules.track_root("invalid-entry-order")
    package = _write_package(
        tmp_path,
        root,
        init=init,
        production=production,
    )

    _assert_load_error(package, "invalid-package-name")


@pytest.mark.parametrize("with_production", [False, True])
def test_missing_init_precedes_missing_production(
    tmp_path: Path, modules: ModuleSandbox, with_production: bool
) -> None:
    root = modules.track_root("missing_init")
    package = _write_package(
        tmp_path,
        root,
        init=None,
        production=_valid_production_source() if with_production else None,
    )

    _assert_load_error(package, "missing-init")


@pytest.mark.parametrize(
    ("with_init", "reason"),
    [(False, "missing-init"), (True, "missing-production")],
)
def test_entry_validation_precedes_existing_name_conflict(
    tmp_path: Path,
    modules: ModuleSandbox,
    with_init: bool,
    reason: str,
) -> None:
    root = modules.track_root("entry_before_conflict")
    package = _write_package(tmp_path, root, init="" if with_init else None)
    conflicting = ModuleType(root)
    conflicting.__spec__ = None
    sys.modules[root] = conflicting

    _assert_load_error(package, reason)

    assert sys.modules[root] is conflicting


def test_missing_production_is_validated_before_package_execution(
    modules: ModuleSandbox,
) -> None:
    root = modules.track_root("missing_entry")
    state = ModuleType("_troupe_missing_entry_state")
    state.executed = False
    modules.install(state.__name__, state)

    _assert_load_error(FIXTURES / root, "missing-production")

    assert state.executed is False
    assert _prefix_modules(root) == {}


@pytest.mark.parametrize(
    ("fixture", "reason"),
    [
        ("missing_symbol", "missing-symbol"),
        ("wrong_base", "symbol-not-subclass"),
    ],
)
def test_static_symbol_validation_failures(
    modules: ModuleSandbox, fixture: str, reason: str
) -> None:
    modules.track_root(fixture)

    _assert_load_error(FIXTURES / fixture, reason)

    assert _prefix_modules(fixture) == {}


@pytest.mark.parametrize(
    ("name", "source", "reason"),
    [
        ("symbol_value", "Production = object()\n", "symbol-not-class"),
        (
            "symbol_base",
            "from troupe import Production as Production\n",
            "symbol-is-base",
        ),
    ],
)
def test_exact_symbol_type_and_base_are_rejected(
    tmp_path: Path,
    modules: ModuleSandbox,
    name: str,
    source: str,
    reason: str,
) -> None:
    modules.track_root(name)
    package = _write_package(tmp_path, name, production=source)

    _assert_load_error(package, reason)


def test_nfkc_stable_non_ascii_package_name_loads(
    tmp_path: Path, modules: ModuleSandbox
) -> None:
    root = modules.track_root("\u751f\u4ea7")
    package = _copy_fixture(tmp_path, "same_name", root)

    instance = _load(package, ["\udcff"])

    assert type(instance).__module__ == f"{root}.production"
    assert instance.received == ["\udcff"]


def _module_with_spec(
    name: str,
    package_dir: Path,
    *,
    origin: str | None,
    locations: list[str] | None,
) -> ModuleType:
    if origin is not None and origin not in {"built-in", "frozen"}:
        spec = importlib.util.spec_from_file_location(
            name,
            origin,
            submodule_search_locations=locations,
        )
        assert spec is not None
        module = importlib.util.module_from_spec(spec)
    else:
        spec = importlib.machinery.ModuleSpec(
            name,
            loader=None,
            origin=origin,
            is_package=locations is not None,
        )
        spec.submodule_search_locations = locations
        module = ModuleType(name)
        module.__spec__ = spec
        module.__package__ = name if locations is not None else name.rpartition(".")[0]
        if locations is not None:
            module.__path__ = locations
    return module


def _basic_package(tmp_path: Path, root: str) -> Path:
    return _write_package(tmp_path, root, production=_valid_production_source())


@pytest.mark.parametrize("attack", ["redirect-path", "inject-module", "preload-other-file"])
def test_loader_executes_the_selected_production_file(
    tmp_path: Path, modules: ModuleSandbox, attack: str
) -> None:
    root = modules.track_root(f"fixed_entry_{attack.replace('-', '_')}")
    outside = tmp_path / "outside"
    outside.mkdir()
    outside_source = (
        "from troupe import Production as BaseProduction\n"
        "class Production(BaseProduction):\n"
        "    def __init__(self, args):\n"
        "        self.source = 'outside'\n"
    )
    (outside / "production.py").write_text(outside_source, encoding="utf-8")
    selected_source = (
        "from troupe import Production as BaseProduction\n"
        "class Production(BaseProduction):\n"
        "    def __init__(self, args):\n"
        "        self.source = 'selected'\n"
    )

    if attack == "redirect-path":
        init = f"__path__ = [{str(outside)!r}]\n"
    elif attack == "inject-module":
        outside_entry = str(outside / "production.py")
        init = f"""
import importlib.util
import sys
spec = importlib.util.spec_from_file_location(__name__ + ".production", {outside_entry!r})
module = importlib.util.module_from_spec(spec)
sys.modules[spec.name] = module
spec.loader.exec_module(module)
production = module
"""
    else:
        init = ""

    package = _write_package(tmp_path, root, init=init, production=selected_source)
    if attack == "preload-other-file":
        fake_path = package / "config.py"
        fake_path.write_text(outside_source, encoding="utf-8")
        name = f"{root}.production"
        spec = importlib.util.spec_from_file_location(name, fake_path)
        assert spec is not None
        assert spec.loader is not None
        fake_module = importlib.util.module_from_spec(spec)
        sys.modules[name] = fake_module
        spec.loader.exec_module(fake_module)

    instance = _load(package, [])

    production_module = sys.modules[f"{root}.production"]
    assert instance.source == "selected"
    assert Path(production_module.__spec__.origin).resolve() == (
        package / "production.py"
    )
    assert sys.modules[root].production is production_module


def test_package_initializer_import_of_selected_production_is_reused(
    tmp_path: Path, modules: ModuleSandbox
) -> None:
    root = modules.track_root("initializer_import_package")
    state = modules.state(executions=0)
    init = "from .production import Production as ExportedProduction\n"
    production = f"""
import {state.__name__} as state
from troupe import Production as BaseProduction
state.executions += 1
class Production(BaseProduction):
    def __init__(self, args):
        self.received = args
"""
    package = _write_package(tmp_path, root, init=init, production=production)

    instance = _load(package, ["value"])

    package_module = sys.modules[root]
    production_module = sys.modules[f"{root}.production"]
    assert state.executions == 1
    assert package_module.production is production_module
    assert package_module.ExportedProduction is production_module.Production
    assert type(instance) is package_module.ExportedProduction
    assert pickle.loads(pickle.dumps(type(instance))) is type(instance)


def test_dynamic_parent_attribute_is_not_resolved_during_fixed_entry_load(
    tmp_path: Path, modules: ModuleSandbox
) -> None:
    root = modules.track_root("dynamic_parent_attribute_package")
    package = _basic_package(tmp_path, root)
    package_module = _module_with_spec(
        root,
        package,
        origin=str(package / "__init__.py"),
        locations=[str(package)],
    )
    calls: list[str] = []

    def dynamic_attribute(name: str) -> object:
        calls.append(name)
        if name == "production":
            return object()
        raise AttributeError(name)

    package_module.__getattr__ = dynamic_attribute
    sys.modules[root] = package_module

    instance = _load(package, ["value"])

    production_module = sys.modules[f"{root}.production"]
    assert calls == []
    assert vars(package_module)["production"] is production_module
    assert instance.received == ["value"]


def test_package_self_replacement_becomes_the_canonical_parent(
    tmp_path: Path, modules: ModuleSandbox
) -> None:
    root = modules.track_root("self_replacing_package")
    state = modules.state(replacement=None)
    init = f"""
import sys
import types
import {state.__name__} as state
replacement = types.ModuleType(__name__)
replacement.__spec__ = __spec__
replacement.__package__ = __package__
replacement.__path__ = __path__
state.replacement = replacement
sys.modules[__name__] = replacement
"""
    package = _write_package(
        tmp_path,
        root,
        init=init,
        production=_valid_production_source(),
    )

    instance = _load(package, ["value"])

    production_module = sys.modules[f"{root}.production"]
    assert sys.modules[root] is state.replacement
    assert state.replacement.production is production_module
    assert type(instance) is production_module.Production
    assert pickle.loads(pickle.dumps(type(instance))) is type(instance)


def test_production_self_replacement_becomes_the_canonical_entry(
    tmp_path: Path, modules: ModuleSandbox
) -> None:
    root = modules.track_root("self_replacing_production")
    state = modules.state(replacement=None)
    production = f"""
import sys
import types
import {state.__name__} as state
from troupe import Production as BaseProduction
class Production(BaseProduction):
    def __init__(self, args):
        self.received = args
replacement = types.ModuleType(__name__)
replacement.__spec__ = __spec__
replacement.__package__ = __package__
replacement.Production = Production
state.replacement = replacement
sys.modules[__name__] = replacement
"""
    package = _write_package(tmp_path, root, production=production)

    instance = _load(package, ["value"])

    assert sys.modules[f"{root}.production"] is state.replacement
    assert sys.modules[root].production is state.replacement
    assert type(instance) is state.replacement.Production
    assert pickle.loads(pickle.dumps(type(instance))) is type(instance)


def test_production_self_replacement_without_symbol_fails_transactionally(
    tmp_path: Path, modules: ModuleSandbox
) -> None:
    root = modules.track_root("invalid_self_replacing_production")
    production = """
import sys
import types
from troupe import Production as BaseProduction
class Production(BaseProduction):
    pass
replacement = types.ModuleType(__name__)
replacement.__spec__ = __spec__
replacement.__package__ = __package__
sys.modules[__name__] = replacement
"""
    package = _write_package(tmp_path, root, production=production)

    _assert_load_error(package, "missing-symbol")

    assert _prefix_modules(root) == {}


def test_symlinked_selected_production_reuses_exact_preloaded_module(
    tmp_path: Path, modules: ModuleSandbox
) -> None:
    root = modules.track_root("symlinked_production_package")
    state = modules.state(executions=0)
    package = _write_package(tmp_path, root)
    target = package / "selected.py"
    target.write_text(
        f"""
import {state.__name__} as state
from troupe import Production as BaseProduction
state.executions += 1
class Production(BaseProduction):
    def __init__(self, args):
        self.received = args
""",
        encoding="utf-8",
    )
    selected = package / "production.py"
    selected.symlink_to(target.name)
    name = f"{root}.production"
    spec = importlib.util.spec_from_file_location(name, target)
    assert spec is not None
    assert spec.loader is not None
    preloaded = importlib.util.module_from_spec(spec)
    sys.modules[name] = preloaded
    spec.loader.exec_module(preloaded)

    instance = _load(package, ["value"])

    assert state.executions == 1
    assert sys.modules[name] is preloaded
    assert sys.modules[root].production is preloaded
    assert type(instance) is preloaded.Production


def test_real_same_name_package_at_another_path_conflicts(
    tmp_path: Path, modules: ModuleSandbox
) -> None:
    root = modules.track_root("same_name")
    first = tmp_path / "first" / root
    second = tmp_path / "second" / root
    shutil.copytree(FIXTURES / root, first)
    shutil.copytree(FIXTURES / root, second)
    _load(first, [])
    before = _prefix_modules(root)

    _assert_load_error(second, "package-name-conflict")

    assert _prefix_modules(root) == before
    assert all(sys.modules[name] is module for name, module in before.items())


@pytest.mark.parametrize(
    "case",
    [
        "no-spec",
        "outside-origin",
        "built-in",
        "frozen",
        "no-locations",
        "empty-locations",
        "mixed-locations",
        "outside-child",
    ],
)
def test_invalid_preloaded_module_ownership_conflicts_without_mutation(
    tmp_path: Path, modules: ModuleSandbox, case: str
) -> None:
    root = modules.track_root("conflict_package")
    package = _basic_package(tmp_path, root)
    outside = tmp_path / "outside"
    outside.mkdir()
    (outside / "module.py").write_text("", encoding="utf-8")

    if case == "no-spec":
        root_module = ModuleType(root)
    elif case == "outside-origin":
        root_module = _module_with_spec(
            root,
            package,
            origin=str(outside / "module.py"),
            locations=[str(package)],
        )
    elif case in {"built-in", "frozen"}:
        root_module = _module_with_spec(
            root, package, origin=case, locations=None
        )
    elif case == "no-locations":
        root_module = _module_with_spec(root, package, origin=None, locations=None)
    elif case == "empty-locations":
        root_module = _module_with_spec(root, package, origin=None, locations=[])
    elif case == "mixed-locations":
        root_module = _module_with_spec(
            root,
            package,
            origin=None,
            locations=[str(package), str(outside)],
        )
    else:
        root_module = _module_with_spec(
            root,
            package,
            origin=str(package / "__init__.py"),
            locations=[str(package)],
        )

    root_module.marker = object()
    sys.modules[root] = root_module
    if case == "outside-child":
        child = _module_with_spec(
            f"{root}.child",
            package,
            origin=str(outside / "module.py"),
            locations=None,
        )
        child.marker = object()
        sys.modules[f"{root}.child"] = child
    before = _prefix_modules(root)
    before_dicts = {name: dict(vars(module)) for name, module in before.items()}

    _assert_load_error(package, "package-name-conflict")

    assert _prefix_modules(root).keys() == before.keys()
    for name, module in before.items():
        assert sys.modules[name] is module
        assert vars(module).keys() == before_dicts[name].keys()
        assert all(vars(module)[key] is value for key, value in before_dicts[name].items())


def test_surrogate_suffix_module_participates_in_conflict_detection(
    tmp_path: Path, modules: ModuleSandbox
) -> None:
    root = modules.track_root("surrogate_conflict_package")
    package = _basic_package(tmp_path, root)
    child_name = f"{root}.\udcff"
    child = ModuleType(child_name)
    child.marker = object()
    sys.modules[child_name] = child

    _assert_load_error(package, "package-name-conflict")

    assert sys.modules[child_name] is child
    assert child.marker is not None


def test_string_subclass_key_cannot_override_prefix_conflict_detection(
    tmp_path: Path, modules: ModuleSandbox
) -> None:
    root = modules.track_root("string_subclass_conflict_package")
    package = _basic_package(tmp_path, root)
    startswith_calls: list[tuple[object, ...]] = []

    class PrefixKey(str):
        def startswith(self, *args: object, **kwargs: object) -> bool:
            startswith_calls.append(args)
            return False

    key = PrefixKey(f"{root}.child")
    child = ModuleType(str(key))
    child.__spec__ = None
    modules.install(key, child)

    _assert_load_error(package, "package-name-conflict")

    assert startswith_calls == []
    assert sys.modules[key] is child


def test_string_subclass_key_cannot_override_rollback_depth(
    tmp_path: Path, modules: ModuleSandbox
) -> None:
    root = modules.track_root("string_subclass_rollback_package")
    count_calls: list[tuple[object, ...]] = []

    class DepthKey(str):
        def count(self, *args: object, **kwargs: object) -> int:
            count_calls.append(args)
            raise RuntimeError("overridden count must not run")

    key = DepthKey(f"{root}.child")
    state = modules.state(replacement=object(), added=object())
    production = f"""
import sys
import {state.__name__} as state
child = sys.modules[__package__ + ".child"]
child.existing = state.replacement
child.added = state.added
"""
    package = _write_package(tmp_path, root, production=production)
    (package / "child.py").write_text("", encoding="utf-8")
    child = _module_with_spec(
        str(key),
        package,
        origin=str(package / "child.py"),
        locations=None,
    )
    child.existing = object()
    modules.install(key, child)
    before = dict(vars(child))

    _assert_load_error(package, "missing-symbol")

    assert count_calls == []
    assert any(name is key for name in sys.modules)
    assert sys.modules[key] is child
    assert vars(child).keys() == before.keys()
    assert all(vars(child)[name] is value for name, value in before.items())


@pytest.mark.parametrize("kind", ["file-origin", "namespace-locations"])
def test_valid_same_path_preloaded_packages_are_reused(
    tmp_path: Path, modules: ModuleSandbox, kind: str
) -> None:
    root = modules.track_root("preloaded_package")
    package = _basic_package(tmp_path, root)
    if kind == "file-origin":
        root_module = _module_with_spec(
            root,
            package,
            origin=str(package / "__init__.py"),
            locations=[str(package)],
        )
    else:
        root_module = _module_with_spec(
            root, package, origin=None, locations=[str(package)]
        )
    root_module.marker = object()
    sys.modules[root] = root_module

    child_name = f"{root}.preloaded"
    (package / "preloaded.py").write_text("", encoding="utf-8")
    child = _module_with_spec(
        child_name,
        package,
        origin=str(package / "preloaded.py"),
        locations=None,
    )
    sys.modules[child_name] = child

    production_name = f"{root}.production"
    production_spec = importlib.util.spec_from_file_location(
        production_name, package / "production.py"
    )
    assert production_spec is not None
    assert production_spec.loader is not None
    preloaded_production = importlib.util.module_from_spec(production_spec)
    sys.modules[production_name] = preloaded_production
    production_spec.loader.exec_module(preloaded_production)

    instance = _load(package, ["value"])

    assert sys.modules[root] is root_module
    assert sys.modules[child_name] is child
    assert sys.modules[production_name] is preloaded_production
    assert root_module.production is preloaded_production
    assert instance.received == ["value"]


def _controlled_failure_package(
    tmp_path: Path,
    modules: ModuleSandbox,
    phase: str,
) -> tuple[Path, ModuleType]:
    root = modules.track_root(f"empty_{phase}_failure")
    original = RuntimeError(f"original-{phase}")
    state = modules.state(phase=phase, original=original, calls=0)
    init = f"""
import {state.__name__} as state
if state.phase == "package":
    raise state.original
"""
    production = f"""
import {state.__name__} as state
from troupe import Production as BaseProduction
if state.phase == "production":
    raise state.original
class Production(BaseProduction):
    def __init__(self, args):
        state.calls += 1
        if state.phase == "construction":
            raise state.original
        self.received = args
"""
    return _write_package(tmp_path, root, init=init, production=production), state


@pytest.mark.parametrize("phase", ["package", "production", "construction"])
def test_empty_prefix_failures_preserve_cause_traceback_and_allow_retry(
    tmp_path: Path, modules: ModuleSandbox, phase: str
) -> None:
    package, state = _controlled_failure_package(tmp_path, modules, phase)
    reason = "construction-failed" if phase == "construction" else "import-failed"

    error = _assert_load_error(package, reason, cause=state.original)

    assert error.__cause__ is state.original
    expected_file = "__init__.py" if phase == "package" else "production.py"
    frames = _traceback_frames(state.original)
    assert any(Path(filename).name == expected_file for filename, _ in frames)
    if phase == "construction":
        assert any(name == "__init__" for _, name in frames)
        assert state.calls == 1
    else:
        assert state.calls == 0
    assert _prefix_modules(package.name) == {}

    state.phase = "success"
    instance = _load(package, ["retry"])
    assert instance.received == ["retry"]
    assert state.calls == (2 if phase == "construction" else 1)


def _package_init_transaction(
    tmp_path: Path, modules: ModuleSandbox, phase: str
) -> tuple[Path, ModuleType, dict[str, ModuleType], dict[str, dict[str, object]]]:
    root = modules.track_root(f"package_init_{phase}_transaction")
    original = RuntimeError(f"package-init-{phase}")
    state = modules.state(
        phase=phase,
        original=original,
        replacement_value=object(),
        added_value=object(),
        replacement_module=ModuleType(f"{root}.replacement"),
    )
    init = f"""
import sys
import types
import {state.__name__} as state
child_name = __name__ + ".child"
child = sys.modules[child_name]
child.existing = state.replacement_value
del child.removed_attr
child.added_attr = state.added_value
sys.modules[child_name] = state.replacement_module
new_module = types.ModuleType(__name__ + ".new")
sys.modules[new_module.__name__] = new_module
if state.phase == "package":
    raise state.original
"""
    production = f"""
import {state.__name__} as state
from troupe import Production as BaseProduction
if state.phase == "validation":
    Production = object()
else:
    class Production(BaseProduction):
        def __init__(self, args):
            self.received = args
"""
    package = _write_package(tmp_path, root, init=init, production=production)
    (package / "child.py").write_text("", encoding="utf-8")
    child = _module_with_spec(
        f"{root}.child",
        package,
        origin=str(package / "child.py"),
        locations=None,
    )
    child.existing = object()
    child.removed_attr = object()
    sys.modules[child.__name__] = child
    assert root not in sys.modules
    mapping = _prefix_modules(root)
    dictionaries = {name: dict(vars(module)) for name, module in mapping.items()}
    return package, state, mapping, dictionaries


@pytest.mark.parametrize("phase", ["package", "validation"])
def test_snapshot_precedes_root_package_execution(
    tmp_path: Path, modules: ModuleSandbox, phase: str
) -> None:
    package, state, mapping, dictionaries = _package_init_transaction(
        tmp_path, modules, phase
    )
    reason = "import-failed" if phase == "package" else "symbol-not-class"
    cause = state.original if phase == "package" else None

    _assert_load_error(package, reason, cause=cause)

    _assert_prefix_restored(package.name, mapping, dictionaries)
    assert package.name not in sys.modules
    state.phase = "success"
    instance = _load(package, ["retry"])
    assert instance.received == ["retry"]


def _transaction_source(state_name: str) -> str:
    return f"""
import argparse
import sys
import types
import {state_name} as state
from troupe import Production as BaseProduction

root_name = __package__
root = sys.modules[root_name]
kept_name = root_name + ".kept"
removed_name = root_name + ".removed"
surrogate_name = root_name + ".\\udcff"
kept = sys.modules[kept_name]
surrogate = sys.modules[surrogate_name]

root.existing = state.replacement_value
del root.removed_attr
root.added_attr = state.added_value
kept.existing = state.replacement_value
del kept.removed_attr
kept.added_attr = state.added_value
surrogate.existing = state.replacement_value
del surrogate.removed_attr
surrogate.added_attr = state.added_value
sys.modules[kept_name] = state.replacement_module
sys.modules[surrogate_name] = state.replacement_surrogate_module
del sys.modules[removed_name]
new_module = types.ModuleType(root_name + ".new")
deep_module = types.ModuleType(root_name + ".new.deep")
surrogate_new_module = types.ModuleType(root_name + ".new\\udcff")
sys.modules[new_module.__name__] = new_module
sys.modules[deep_module.__name__] = deep_module
sys.modules[surrogate_new_module.__name__] = surrogate_new_module
root.new = new_module

if state.phase == "import":
    raise state.original

if state.phase == "validation":
    Production = object()
else:
    class Production(BaseProduction):
        def __init__(self, args):
            state.calls += 1
            if state.phase == "construction":
                raise state.original
            if state.phase == "argparse":
                parser = argparse.ArgumentParser(prog="transaction-production")
                parser.add_argument("--value")
                try:
                    self.options = parser.parse_args(args)
                except SystemExit as error:
                    state.system_exit = error
                    raise
            self.received = args
"""


def _transaction_package(
    tmp_path: Path, modules: ModuleSandbox, phase: str
) -> tuple[Path, ModuleType, dict[str, ModuleType], dict[str, dict[str, object]]]:
    root = modules.track_root(f"transaction_{phase}_package")
    original = RuntimeError(f"transaction-{phase}")
    state = modules.state(
        phase=phase,
        original=original,
        calls=0,
        system_exit=None,
        replacement_value=object(),
        added_value=object(),
        replacement_module=ModuleType(f"{root}.replacement"),
        replacement_surrogate_module=ModuleType(f"{root}.replacement_surrogate"),
    )
    package = _write_package(
        tmp_path,
        root,
        init="",
        production=_transaction_source(state.__name__),
    )
    root_module = _module_with_spec(
        root,
        package,
        origin=str(package / "__init__.py"),
        locations=[str(package)],
    )
    kept = _module_with_spec(
        f"{root}.kept",
        package,
        origin=str(package / "kept.py"),
        locations=None,
    )
    removed = _module_with_spec(
        f"{root}.removed",
        package,
        origin=str(package / "removed.py"),
        locations=None,
    )
    surrogate_name = f"{root}.\udcff"
    (package / "surrogate.py").write_text("", encoding="utf-8")
    surrogate = _module_with_spec(
        surrogate_name,
        package,
        origin=str(package / "surrogate.py"),
        locations=None,
    )
    root_module.existing = object()
    root_module.removed_attr = object()
    root_module.kept = kept
    root_module.removed = removed
    kept.existing = object()
    kept.removed_attr = object()
    surrogate.existing = object()
    surrogate.removed_attr = object()
    sys.modules[root] = root_module
    sys.modules[kept.__name__] = kept
    sys.modules[removed.__name__] = removed
    sys.modules[surrogate_name] = surrogate

    neighbor = ModuleType(f"{root}_extra")
    neighbor.marker = object()
    modules.install(neighbor.__name__, neighbor)
    state.neighbor = neighbor
    state.neighbor_snapshot = dict(vars(neighbor))

    mapping = _prefix_modules(root)
    dictionaries = {name: dict(vars(module)) for name, module in mapping.items()}
    return package, state, mapping, dictionaries


def _assert_prefix_restored(
    root: str,
    mapping: dict[str, ModuleType],
    dictionaries: dict[str, dict[str, object]],
) -> None:
    current = _prefix_modules(root)
    assert current.keys() == mapping.keys()
    for name, old_module in mapping.items():
        assert current[name] is old_module
        old_dict = dictionaries[name]
        current_dict = vars(old_module)
        assert current_dict.keys() == old_dict.keys()
        assert all(current_dict[key] is value for key, value in old_dict.items())


def _assert_transaction_restored(
    root: str,
    state: ModuleType,
    mapping: dict[str, ModuleType],
    dictionaries: dict[str, dict[str, object]],
) -> None:
    _assert_prefix_restored(root, mapping, dictionaries)
    assert vars(state.neighbor).keys() == state.neighbor_snapshot.keys()
    assert all(
        vars(state.neighbor)[key] is value
        for key, value in state.neighbor_snapshot.items()
    )
    assert sys.modules[state.neighbor.__name__] is state.neighbor


@pytest.mark.parametrize("phase", ["import", "validation", "construction"])
def test_preloaded_prefix_mapping_and_module_dicts_are_transactional(
    tmp_path: Path, modules: ModuleSandbox, phase: str
) -> None:
    package, state, mapping, dictionaries = _transaction_package(
        tmp_path, modules, phase
    )
    reason = {
        "import": "import-failed",
        "validation": "symbol-not-class",
        "construction": "construction-failed",
    }[phase]
    cause = state.original if phase in {"import", "construction"} else None

    _assert_load_error(package, reason, cause=cause)

    _assert_transaction_restored(package.name, state, mapping, dictionaries)
    assert state.calls == (1 if phase == "construction" else 0)

    state.phase = "success"
    instance = _load(package, ["retry"])
    assert instance.received == ["retry"]


@pytest.mark.parametrize(
    ("args", "code"),
    [(["--help"], 0), (["--unknown"], 2)],
)
def test_constructor_system_exit_is_unwrapped_and_rolls_back(
    tmp_path: Path,
    modules: ModuleSandbox,
    capsys: pytest.CaptureFixture[str],
    args: list[str],
    code: int,
) -> None:
    package, state, mapping, dictionaries = _transaction_package(
        tmp_path, modules, "argparse"
    )

    with pytest.raises(SystemExit) as caught:
        _load(package, args)

    assert caught.value is state.system_exit
    assert caught.value.code == code
    captured = capsys.readouterr()
    if code == 0:
        assert "usage: transaction-production" in captured.out
        assert captured.err == ""
    else:
        assert captured.out == ""
        assert "usage: transaction-production" in captured.err
        assert "unrecognized arguments: --unknown" in captured.err
    assert state.calls == 1
    _assert_transaction_restored(package.name, state, mapping, dictionaries)

    instance = _load(package, ["--value", "ok"])
    assert instance.options.value == "ok"
    assert state.calls == 2
