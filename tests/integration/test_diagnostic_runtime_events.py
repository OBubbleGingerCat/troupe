from __future__ import annotations

import os
import subprocess
import sysconfig
from pathlib import Path


ROOT = Path(__file__).resolve().parents[2]
PRODUCER = ROOT / "rust/src/diagnostic_runtime/runtime_producer.rs"
RUNTIME = ROOT / "rust/src/orchestration/runtime.rs"


def test_runtime_taxonomy_and_stable_error_codes_are_closed() -> None:
    source = PRODUCER.read_text(encoding="utf-8")

    for span in (
        "SpanStartDetail::RunLifecycle",
        "SpanStartDetail::ProductionStart",
        "SpanStartDetail::ProductionStop",
        "SpanStartDetail::ProductionShutdown",
    ):
        assert span in source
    for code in (
        "production-start-cancelled",
        "production-start-failed",
        "production-scene-failed",
        "production-stop-cancelled",
        "production-stop-failed",
        "production-lifecycle-cancelled",
        "production-lifecycle-failed",
        "production-shutdown-cancelled",
        "production-shutdown-failed",
    ):
        assert f'"{code}"' in source


def test_real_runtime_hooks_include_authoritative_terminal_result() -> None:
    runtime = RUNTIME.read_text(encoding="utf-8")

    assert runtime.count("RuntimeHook::RunLifecycleReturned") == 2
    early_result = runtime.index("let result = lifecycle_result(failures);")
    early_observe = runtime.index("RuntimeHook::RunLifecycleReturned", early_result)
    early_return = runtime.index("return result;", early_observe)
    assert early_result < early_observe < early_return

    final_result = runtime.index("let result = lifecycle_result(failures);", early_return)
    final_observe = runtime.index("RuntimeHook::RunLifecycleReturned", final_result)
    final_return = runtime.index("result", final_observe)
    assert final_result < final_observe < final_return


def test_runtime_producer_is_finite_and_does_not_capture_user_payloads() -> None:
    source = PRODUCER.read_text(encoding="utf-8")

    for required in (
        "producer_failure: Option<RuntimeProducerError>",
        "first_user_failure: Option<TerminalSpan>",
        "runtime.run-finished-before-lifecycle-return",
        "producer_for_binding",
    ):
        assert required in source

    for forbidden in (
        "raw_exception",
        "exception_message",
        "traceback",
        "script",
        "production_args",
    ):
        assert forbidden not in source


def test_native_runtime_producer_contract() -> None:
    command = [
        "cargo",
        "test",
        "--locked",
        "--manifest-path",
        "rust/Cargo.toml",
        "--package",
        "troupe",
        "diagnostic_runtime::runtime_producer::tests",
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
