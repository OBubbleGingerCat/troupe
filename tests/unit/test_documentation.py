from __future__ import annotations

import ast
import doctest
import inspect
import json
import os
import re
import secrets
import select
import signal
import subprocess
import sys
from pathlib import Path

import troupe


ROOT = Path(__file__).resolve().parents[2]
README = ROOT / "README.md"
EXAMPLE_START = "<!-- BEGIN README PRODUCTION -->"
EXAMPLE_END = "<!-- END README PRODUCTION -->"
TIMEOUT = 5.0
READY_PREFIX = b"troupe: diagnostic ready "


def _readme() -> str:
    return README.read_text(encoding="utf-8")


def _production_source() -> str:
    text = _readme()
    assert text.count(EXAMPLE_START) == 1
    assert text.count(EXAMPLE_END) == 1
    match = re.search(
        rf"{re.escape(EXAMPLE_START)}\n```python\n(?P<source>.*?)```\n"
        rf"{re.escape(EXAMPLE_END)}",
        text,
        flags=re.DOTALL,
    )
    assert match is not None
    return match.group("source")


def _without_ready(stderr: bytes, package: Path) -> bytes:
    ready, separator, remaining = stderr.partition(b"\n")
    assert separator == b"\n"
    assert ready.startswith(READY_PREFIX)
    assert READY_PREFIX not in remaining
    locator = json.loads(ready.removeprefix(READY_PREFIX))
    assert locator["locator_schema_version"] == 1
    assert type(locator["run_id"]) is str and locator["run_id"]
    assert locator["local_url"].startswith("http://127.0.0.1:")
    assert locator["advertise_url"] is None
    archive = Path(locator["archive_directory"])
    assert archive.is_absolute()
    assert archive.is_relative_to((package / ".troupe").resolve())
    assert locator["security_scope"] == "trusted_network"
    return remaining


def test_readme_documents_installation_direct_command_and_ownership() -> None:
    text = _readme()

    assert "pip install troupe" in text
    assert "troupe --production" in text
    assert "does not require `uv run`" in text
    assert "`uv run troupe` only selects the project environment" in text
    assert "one thin `troupe/__init__.py` wrapper" in text
    assert "implemented in the Rust extension" in text
    assert "`troupe/__init__.pyi`" in text
    for name in (
        "Actor",
        "ActorHandle",
        "Cue",
        "CueContextError",
        "Effect",
        "EffectContextError",
        "Production",
    ):
        assert f"`{name}`" in text


def test_readme_documents_argv_lifecycle_cancellation_and_failures() -> None:
    text = _readme()

    assert "equivalent to `sys.argv[1:]`" in text
    assert "tokens after `--`" in text
    assert "synchronous constructor" in text
    assert "async `start()`, `scene()`, and `stop()`" in text
    assert "swallows `CancelledError`" in text
    assert "start, scene, and stop are separate failure phases" in text


def test_readme_documents_scene_lineage_and_cue_runner_boundaries() -> None:
    text = _readme()
    boundaries = (
        "last live `ActorHandle`",
        "exact string or a compiled regular expression",
        "legal only from an active Scene",
        "registered task lineage",
        "Direct `asyncio.Task(...)` construction is not supported",
        "replacing the event loop task factory makes the scene phase fail",
        "exactly one consumer",
        "concurrent double-await while pending is not supported",
        "cannot reuse already awaited coroutine",
        "does not guarantee `CancelledError` identity, arguments,\n"
        "traceback, or `__context__` chain shape",
        "shallow copy",
        "read-only mapping",
        "scene UUID",
        "`-cue0`",
        "`-effect0`",
        "strict FIFO order",
        "different Actors progress cooperatively",
        "no mailbox capacity or backpressure",
        "cancels and drains",
        "no cancellation grace period",
        "dependency cycle between Actors",
        "user-defined Effect fields remain mutable",
        "does not consume user Effects",
        "Scene-owned work completes or is cancelled before scene returns",
        "Cross-scene work is managed by start and stop",
        "does not define a subtask or task-group API",
        "`Runtime` is not a public programmatic API",
    )
    for boundary in boundaries:
        assert boundary in text


def test_readme_documents_actor_act_schema_and_callback_boundaries() -> None:
    text = _readme()
    boundaries = (
        "`Actor.act()`",
        "one persistent ACP session",
        "already be logged in",
        "Node.js",
        "npm",
        "`npx`",
        "Kimi Code 0.31.1",
        "`description`",
        "`choices`",
        "`ObjectValue`",
        "multiple disjoint ranges",
        "async database validator",
        "`ValueRejected`",
        "idempotent",
        "no Runtime timeout",
        "`asyncio.CancelledError`",
        "`SchemaCallbackError`",
        "`phase`",
        "`path`",
    )
    for boundary in boundaries:
        assert boundary in text

    schema_stub = (ROOT / "src" / "troupe" / "act_schema.pyi").read_text(
        encoding="utf-8"
    )
    public_stub = (ROOT / "src" / "troupe" / "__init__.pyi").read_text(
        encoding="utf-8"
    )
    assert "Validation callbacks may be synchronous or asynchronous" in schema_stub
    assert "Return one validated JSON object from this Actor's persistent agent session" in public_stub


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
    source = _production_source()
    assert "troupe._runtime" not in source
    assert "_Runtime" not in source
    tree = ast.parse(source)

    troupe_imports = [
        node
        for node in tree.body
        if isinstance(node, ast.Import)
        and [(alias.name, alias.asname) for alias in node.names] == [("troupe", None)]
    ]
    assert len(troupe_imports) == 1
    public_names = {
        "AgentProfile",
        "Actor",
        "ActorHandle",
        "Cue",
        "CueContextError",
        "Effect",
        "EffectContextError",
        "Production",
    }
    for node in ast.walk(tree):
        if not isinstance(node, ast.Attribute):
            continue
        value = node.value
        while isinstance(value, ast.Attribute):
            value = value.value
        if isinstance(value, ast.Name) and value.id == "troupe":
            assert node.attr in public_names

    classes = [node for node in tree.body if isinstance(node, ast.ClassDef)]
    bases = [
        ast.unparse(base)
        for definition in classes
        for base in definition.bases
    ]
    assert bases.count("troupe.Actor") == 1
    assert bases.count("troupe.Effect") == 1
    assert bases.count("troupe.Production") == 1
    actor_class = next(
        definition
        for definition in classes
        if [ast.unparse(base) for base in definition.bases] == ["troupe.Actor"]
    )
    production_class = next(
        definition
        for definition in classes
        if [ast.unparse(base) for base in definition.bases] == ["troupe.Production"]
    )
    assert any(
        isinstance(node, ast.AsyncFunctionDef) and node.name == "cued"
        for node in actor_class.body
    )
    assert any(
        isinstance(node, ast.AsyncFunctionDef) and node.name == "scene"
        for node in production_class.body
    )

    calls = [node for node in ast.walk(tree) if isinstance(node, ast.Call)]
    cast_calls = [
        call
        for call in calls
        if isinstance(call.func, ast.Attribute) and call.func.attr == "cast_actor"
    ]
    assert len(cast_calls) >= 2
    for call in cast_calls:
        assert call.args
        assert {keyword.arg for keyword in call.keywords} == {
            "name",
            "agent_profile",
            "actor_args",
            "actor_kwargs",
        }
    assert any(
        isinstance(call.func, ast.Attribute) and call.func.attr == "make_effect"
        for call in calls
    )
    assert any(
        isinstance(call.func, ast.Attribute) and call.func.attr == "cue"
        for call in calls
    )
    assert any(ast.unparse(call.func) == "os.write" for call in calls)
    assert any(ast.unparse(call.func) == "json.dumps" for call in calls)

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
    assert parsed.globs["production_source"] == source
    example_type = parsed.globs["production_type"]
    example = parsed.globs["example"]
    assert inspect.isclass(example_type)
    assert issubclass(example_type, troupe.Production)
    assert type(example) is example_type
    assert isinstance(example, example_type)
    exact = example.get_actor("greeter")
    assert exact is not None
    assert exact.name == "greeter"
    assert [handle.name for handle in example.get_actor(re.compile(r"(?:greeter|writer)"))] == [
        "greeter",
        "writer",
    ]

def test_readme_example_runs_through_literal_console_and_stops_on_sigint(
    tmp_path: Path,
) -> None:
    package = tmp_path / "readme_production"
    package.mkdir()
    (package / "__init__.py").write_text("", encoding="utf-8")
    (package / "production.py").write_text(_production_source(), encoding="utf-8")
    outside = tmp_path / "outside-repository"
    outside.mkdir()

    console = Path(sys.executable).with_name("troupe")
    assert console.is_file()
    env = os.environ.copy()
    for name in ("CONDA_PREFIX", "PYTHONHOME", "PYTHONPATH", "VIRTUAL_ENV"):
        env.pop(name, None)
    env["PYTHONDONTWRITEBYTECODE"] = "1"
    message = f"hello-{secrets.token_hex(8)}"

    read_fd, write_fd = os.pipe()
    process = subprocess.Popen(
        [str(console), "--production", str(package), "--", str(write_fd), message],
        cwd=outside,
        env=env,
        pass_fds=(write_fd,),
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
    )
    os.close(write_fd)
    try:
        readable, _, _ = select.select([read_fd], [], [], TIMEOUT)
        assert readable, "README Production did not report Effect receipt"
        payload = json.loads(os.read(read_fd, 256))
        assert isinstance(payload, list)
        assert payload[:4] == ["tuple", 1, True, True]
        assert re.fullmatch(
            r"scene-[0-9a-f]{8}-[0-9a-f]{4}-4[0-9a-f]{3}-"
            r"[89ab][0-9a-f]{3}-[0-9a-f]{12}-cue0-effect0",
            payload[4],
        )
        assert payload[5:] == ["greeter", message]

        process.send_signal(signal.SIGINT)
        stdout, stderr = process.communicate(timeout=TIMEOUT)
        assert process.returncode == 0, stdout.decode() + stderr.decode()
        assert _without_ready(stderr, package) == b""
    finally:
        os.close(read_fd)
        if process.poll() is None:
            process.kill()
        process.communicate(timeout=TIMEOUT)
