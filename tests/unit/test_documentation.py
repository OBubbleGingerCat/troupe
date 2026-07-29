from __future__ import annotations

import doctest
import inspect
from pathlib import Path

import troupe


ROOT = Path(__file__).resolve().parents[2]
README = ROOT / "README.md"


def _readme() -> str:
    return README.read_text(encoding="utf-8")


def test_readme_documents_installation_direct_command_and_ownership() -> None:
    text = _readme()

    assert "pip install troupe" in text
    assert "troupe --production" in text
    assert "does not require `uv run`" in text
    assert "`uv run troupe` only selects the project environment" in text
    assert "one thin `troupe/__init__.py` wrapper" in text
    assert "implemented in the Rust extension" in text
    assert "`troupe/__init__.pyi`" in text
    assert "only public Python API" in text


def test_readme_documents_argv_lifecycle_cancellation_and_failures() -> None:
    text = _readme()

    assert "equivalent to `sys.argv[1:]`" in text
    assert "tokens after `--`" in text
    assert "synchronous constructor" in text
    assert "async `start()`, `scene()`, and `stop()`" in text
    assert "no cancellation grace period" in text
    assert "swallows `CancelledError`" in text
    assert "start, scene, and stop are separate failure phases" in text


def test_readme_names_all_six_async_work_responsibilities() -> None:
    text = _readme()
    responsibilities = (
        "Scene is the only runtime-defined async work boundary",
        "The runtime manages only the top-level scene task",
        "Scene-owned work completes or is cancelled before scene returns",
        "Cancellation is propagated and cleanup is awaited",
        "Cross-scene work is managed by start and stop",
        "Production chooses gather or another compatible task library",
    )

    for responsibility in responsibilities:
        assert responsibility in text
    assert "Troupe does not define a subtask or task-group API" in text


def test_readme_names_all_four_package_identity_rules_and_linux_scope() -> None:
    text = _readme()
    identity_rules = (
        "The directory basename is the real Python package name",
        "Relative and absolute package imports keep the same identity",
        "Package resources are available through importlib.resources",
        "Module and pickle identity use `<package>.production.Production`",
    )

    for rule in identity_rules:
        assert rule in text
    assert "a valid, non-keyword Python identifier" in text
    assert "unchanged by NFKC normalization" in text
    assert "Linux x86_64 glibc only" in text
    assert "manylinux_2_17_x86_64" in text
    assert "free-threaded CPython is not supported" in text
    assert "macOS, Windows, musllinux, and other architectures are not supported" in text


def test_native_production_and_hooks_have_responsibility_docstrings() -> None:
    production_doc = troupe.Production.__doc__ or ""
    start_doc = troupe.Production.start.__doc__ or ""
    scene_doc = troupe.Production.scene.__doc__ or ""
    stop_doc = troupe.Production.stop.__doc__ or ""

    assert "synchronous" in production_doc
    assert "raw argument" in production_doc
    assert "asynchronous resources" in start_doc
    assert "one scene" in scene_doc
    assert "top-level" in scene_doc
    assert "release" in stop_doc
    assert "await" in stop_doc


def test_readme_doctest_exercises_the_actual_native_base() -> None:
    text = _readme()
    assert "class ExampleProduction(troupe.Production):" in text

    result = doctest.testfile(
        str(README),
        module_relative=False,
        optionflags=doctest.ELLIPSIS,
    )
    assert result.failed == 0
    assert result.attempted >= 6

    globs: dict[str, object] = {}
    parsed = doctest.DocTestParser().get_doctest(
        text,
        globs,
        "README-native-example",
        str(README),
        0,
    )
    runner = doctest.DocTestRunner(optionflags=doctest.ELLIPSIS)
    runner.run(parsed, clear_globs=False)
    assert runner.summarize().failed == 0
    example_type = parsed.globs["ExampleProduction"]
    example = parsed.globs["example"]
    assert inspect.isclass(example_type)
    assert issubclass(example_type, troupe.Production)
    assert type(example) is example_type
    assert isinstance(example, example_type)
    assert example.options.value == 7
    assert "super" not in example_type.__init__.__code__.co_names
    assert "Production" not in example_type.__init__.__code__.co_names
