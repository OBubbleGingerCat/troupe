from __future__ import annotations

import os
import subprocess
import sysconfig
from pathlib import Path


ROOT = Path(__file__).resolve().parents[2]
PRODUCER = ROOT / "rust/src/diagnostic_runtime/scene_producer.rs"
PYTHON_TASK = ROOT / "rust/src/orchestration/python_task.rs"


def test_scene_producer_uses_one_lifecycle_span_and_immutable_scope() -> None:
    source = PRODUCER.read_text(encoding="utf-8")

    for required in (
        "SpanStartDetail::SceneLifecycle",
        "Some(scene_id)",
        "Some(run_span_id)",
        "task_terminal: Option<SceneTerminal>",
        "cleanup_finished: bool",
        "SceneLineageSnapshot",
        "current_scene_snapshot",
        ".current_lineage(py)?",
        "lineage.is_active()",
    ):
        assert required in source
    assert "CounterSampled" not in source
    assert "scene.active" not in source


def test_scene_task_creation_failure_reports_terminal_before_return() -> None:
    source = PYTHON_TASK.read_text(encoding="utf-8")
    invoke_start = source.index("impl SceneTaskCallback")
    invoke_end = source.index("impl RunBindingCallback", invoke_start)
    invoke = source[invoke_start:invoke_end]

    created = invoke.index("created_scene")
    result = invoke.index("let result =")
    terminal = invoke.index("scene_producer::task_finished", result)
    close = invoke.index("scene.close();", terminal)
    send = invoke.index("sender.send(result)", close)
    assert created < result < terminal < close < send


def test_scene_events_do_not_capture_python_failure_payloads() -> None:
    source = PRODUCER.read_text(encoding="utf-8")

    for code in ("scene-lifecycle-cancelled", "scene-lifecycle-failed"):
        assert f'"{code}"' in source
    for forbidden in (
        "raw_exception",
        "exception_message",
        "traceback",
        "script",
        "production_args",
    ):
        assert forbidden not in source


def test_native_scene_producer_contract() -> None:
    command = [
        "cargo",
        "test",
        "--locked",
        "--manifest-path",
        "rust/Cargo.toml",
        "--package",
        "troupe",
        "diagnostic_runtime::scene_producer::tests",
        "--",
        "--nocapture",
    ]
    environment = os.environ.copy()
    libdir = sysconfig.get_config_var("LIBDIR")
    if libdir:
        current = environment.get("LD_LIBRARY_PATH")
        environment["LD_LIBRARY_PATH"] = (
            f"{libdir}{os.pathsep}{current}" if current else str(libdir)
        )
    subprocess.run(command, cwd=ROOT, env=environment, check=True)
