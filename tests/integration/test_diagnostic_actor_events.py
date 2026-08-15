from __future__ import annotations

import os
import subprocess
import sysconfig
from pathlib import Path


ROOT = Path(__file__).resolve().parents[2]
PRODUCER = ROOT / "rust/src/diagnostic_runtime/actor_producer.rs"


def test_actor_producer_uses_stable_scope_and_constructor_containment() -> None:
    source = PRODUCER.read_text(encoding="utf-8")

    for required in (
        "InstantDetail::ActorCast",
        "SpanStartDetail::ActorHandleLifetime",
        "ActorLineageSnapshot",
        "current_production_construction",
        "construction.construct_span_id()",
        "Some(runtime.run_span_id())",
        "actor_scope(actor_id)",
        "session_generation().is_none()",
        "ActorHook::RegistryDetached",
    ):
        assert required in source
    assert 'format!("actor-{value}")' in source
    assert "identity_address()" not in source
    assert 'format!("{:p}")' not in source


def test_actor_producer_stays_out_of_cue_act_and_agent_session_protocols() -> None:
    source = PRODUCER.read_text(encoding="utf-8")

    for forbidden in (
        "InstantDetail::Cue",
        "InstantDetail::ActAdmitted",
        "InstantDetail::ActWaitingReady",
        "InstantDetail::ActPromptSubmitted",
        "SpanStartDetail::ActLifecycle",
        "SpanStartDetail::ActCaller",
        "AgentSessionOpening",
        "AgentSessionLifecycle",
        "AgentSessionClosing",
        "AgentMessageDelta",
        "ToolCall",
    ):
        assert forbidden not in source
    for secret_payload in (
        "agent_profile",
        "raw_exception",
        "exception_message",
        "traceback",
        "tool_input",
        "tool_output",
    ):
        assert secret_payload not in source


def test_native_actor_producer_contract() -> None:
    command = [
        "cargo",
        "test",
        "--locked",
        "--manifest-path",
        "rust/Cargo.toml",
        "--package",
        "troupe",
        "diagnostic_runtime::actor_producer::tests",
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
