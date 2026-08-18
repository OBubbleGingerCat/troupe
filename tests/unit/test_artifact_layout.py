from __future__ import annotations

import ast
import base64
import contextlib
import csv
import hashlib
import importlib
import importlib.util
import importlib.metadata
import io
import json
import os
import re
import shutil
import stat
import subprocess
import sys
import sysconfig
import tarfile
import warnings
import zipfile
from abc import ABC, abstractmethod
from collections.abc import Generator
from dataclasses import dataclass
from pathlib import Path
from types import ModuleType, SimpleNamespace
from typing import Any

import pytest
from wheel.wheelfile import WheelFile

try:
    import tomllib
except ModuleNotFoundError:
    import tomli as tomllib


ROOT = Path(__file__).resolve().parents[2]
PACKAGE = ROOT / "src" / "troupe"
EXPECTED_RUST_SOURCES = [
    "crates/troupe-agent-runtime/src/adapter/claude.rs",
    "crates/troupe-agent-runtime/src/adapter/codex.rs",
    "crates/troupe-agent-runtime/src/adapter/kimi.rs",
    "crates/troupe-agent-runtime/src/adapter/mod.rs",
    "crates/troupe-agent-runtime/src/error.rs",
    "crates/troupe-agent-runtime/src/launch/fd_registry.rs",
    "crates/troupe-agent-runtime/src/launch/mod.rs",
    "crates/troupe-agent-runtime/src/launch/process.rs",
    "crates/troupe-agent-runtime/src/lib.rs",
    "crates/troupe-agent-runtime/src/profile.rs",
    "crates/troupe-agent-runtime/src/result/mod.rs",
    "crates/troupe-agent-runtime/src/result/tests.rs",
    "crates/troupe-agent-runtime/src/schema/mod.rs",
    "crates/troupe-agent-runtime/src/schema/tests.rs",
    "crates/troupe-agent-runtime/src/schema/validation_bridge.rs",
    "crates/troupe-agent-runtime/src/session/mod.rs",
    "crates/troupe-agent-runtime/src/session/supervisor.rs",
    "crates/troupe-agent-runtime/src/session/tests.rs",
    "crates/troupe-agent-runtime/src/session/turn.rs",
    "src/act_call.rs",
    "src/application/cli.rs",
    "src/application/diagnostics.rs",
    "src/application/failure.rs",
    "src/application/invocation.rs",
    "src/application/loader.rs",
    "src/application/mod.rs",
    "src/application/signals.rs",
    "src/lib.rs",
    "src/orchestration/actor.rs",
    "src/orchestration/actor_handle.rs",
    "src/orchestration/actor_registry.rs",
    "src/orchestration/cue.rs",
    "src/orchestration/cue_future.rs",
    "src/orchestration/effect.rs",
    "src/orchestration/mailbox.rs",
    "src/orchestration/mod.rs",
    "src/orchestration/production.rs",
    "src/orchestration/python_task.rs",
    "src/orchestration/runtime.rs",
    "src/orchestration/scene_context.rs",
]
SYNTHETIC_RUST_BUILD_INPUTS = {
    "rust/Cargo.lock": b"version = 4\n",
    "rust/Cargo.toml": b'[workspace]\nmembers = ["crates/troupe-agent-runtime"]\n',
    "rust/crates/troupe-agent-runtime/Cargo.toml": (
        b'[package]\nname = "troupe-agent-runtime"\npublish = false\n'
    ),
    "rust/crates/troupe-agent-runtime/src/lib.rs": b"pub fn agent_runtime() {}\n",
    "rust/src/lib.rs": b"pub fn runtime() {}\n",
}

EXPECTED_WRAPPER = (
    b"from dataclasses import dataclass as _dataclass\n"
    b"from os import PathLike as _PathLike\n"
    b"from typing import Literal as _Literal\n"
    b"\n"
    b"from ._runtime import Actor as Actor\n"
    b"from ._runtime import ActorHandle as ActorHandle\n"
    b"from ._runtime import AgentAuthenticationRequiredError as AgentAuthenticationRequiredError\n"
    b"from ._runtime import AgentError as AgentError\n"
    b"from ._runtime import AgentResultError as AgentResultError\n"
    b"from ._runtime import AgentResultIssue as AgentResultIssue\n"
    b"from ._runtime import AgentResultMissingError as AgentResultMissingError\n"
    b"from ._runtime import AgentSessionBrokenError as AgentSessionBrokenError\n"
    b"from ._runtime import AgentSessionBusyError as AgentSessionBusyError\n"
    b"from ._runtime import AgentSessionError as AgentSessionError\n"
    b"from ._runtime import AgentSessionStartError as AgentSessionStartError\n"
    b"from ._runtime import AgentTurnError as AgentTurnError\n"
    b"from ._runtime import act_schema as act_schema\n"
    b"from ._runtime import Cue as Cue\n"
    b"from ._runtime import CueContextError as CueContextError\n"
    b"from ._runtime import Effect as Effect\n"
    b"from ._runtime import EffectContextError as EffectContextError\n"
    b"from ._runtime import Production as Production\n"
    b"\n"
    b"\n"
    b"@_dataclass(frozen=True, slots=True, kw_only=True)\n"
    b"class AgentProfile:\n"
    b'    agent: _Literal["codex", "claude", "kimi"]\n'
    b"    workspace: str | _PathLike[str]\n"
    b"    model: str\n"
    b"    effort: str | None\n"
    b"\n"
    b"    def __post_init__(self) -> None:\n"
    b"        if not isinstance(self.agent, str):\n"
    b'            raise TypeError("agent must be a str")\n'
    b'        if self.agent not in {"codex", "claude", "kimi"}:\n'
    b"            raise ValueError(\"agent must be one of: 'codex', 'claude', 'kimi'\")\n"
    b"        if not isinstance(self.model, str):\n"
    b'            raise TypeError("model must be a str")\n'
    b"        if not self.model:\n"
    b'            raise ValueError("model must not be empty")\n'
    b"        if self.effort is not None and not isinstance(self.effort, str):\n"
    b'            raise TypeError("effort must be a str or None")\n'
    b'        if self.effort == "":\n'
    b'            raise ValueError("effort must not be empty")\n'
    b"\n"
    b"\n"
    b"__all__ = [\n"
    b'    "Actor",\n'
    b'    "ActorHandle",\n'
    b'    "AgentAuthenticationRequiredError",\n'
    b'    "AgentError",\n'
    b'    "AgentProfile",\n'
    b'    "AgentResultError",\n'
    b'    "AgentResultIssue",\n'
    b'    "AgentResultMissingError",\n'
    b'    "AgentSessionBrokenError",\n'
    b'    "AgentSessionBusyError",\n'
    b'    "AgentSessionError",\n'
    b'    "AgentSessionStartError",\n'
    b'    "AgentTurnError",\n'
    b'    "Cue",\n'
    b'    "CueContextError",\n'
    b'    "Effect",\n'
    b'    "EffectContextError",\n'
    b'    "Production",\n'
    b'    "act_schema",\n'
    b"]\n"
)
EXPECTED_STUB = (
    b"from __future__ import annotations\n"
    b"\n"
    b"from collections.abc import Mapping\n"
    b"from dataclasses import dataclass\n"
    b"from os import PathLike\n"
    b"from re import Pattern\n"
    b"from typing import Any, Literal, NoReturn, TypeVar, final, overload\n"
    b"from typing_extensions import disjoint_base\n"
    b"\n"
    b"from . import act_schema as act_schema\n"
    b"\n"
    b'_EffectT = TypeVar("_EffectT", bound="Effect")\n'
    b'_JsonValue = None | bool | int | float | str | list["_JsonValue"] | dict[str, "_JsonValue"]\n'
    b"\n"
    b"class AgentError(RuntimeError):\n"
    b"    code: str\n"
    b"\n"
    b"class AgentSessionBusyError(AgentError): ...\n"
    b"\n"
    b"class AgentSessionError(AgentError): ...\n"
    b"\n"
    b"class AgentSessionStartError(AgentSessionError):\n"
    b"    phase: str\n"
    b"\n"
    b"class AgentAuthenticationRequiredError(AgentSessionStartError): ...\n"
    b"\n"
    b"class AgentSessionBrokenError(AgentSessionError): ...\n"
    b"\n"
    b"class AgentTurnError(AgentError): ...\n"
    b"\n"
    b"@final\n"
    b"class AgentResultIssue:\n"
    b"    def __new__(cls, _token: NoReturn, /) -> AgentResultIssue: ...\n"
    b"    @property\n"
    b"    def path(self) -> str: ...\n"
    b"    @property\n"
    b"    def code(self) -> str: ...\n"
    b"    @property\n"
    b"    def message(self) -> str: ...\n"
    b"\n"
    b"class AgentResultError(AgentTurnError):\n"
    b"    issues: tuple[AgentResultIssue, ...]\n"
    b"    invalid_calls: int\n"
    b"    details_truncated: bool\n"
    b"\n"
    b"class AgentResultMissingError(AgentResultError): ...\n"
    b"\n"
    b"@dataclass(frozen=True, slots=True, kw_only=True)\n"
    b"class AgentProfile:\n"
    b'    agent: Literal["codex", "claude", "kimi"]\n'
    b"    workspace: str | PathLike[str]\n"
    b"    model: str\n"
    b"    effort: str | None\n"
    b"    def __post_init__(self) -> None: ...\n"
    b"\n"
    b"@disjoint_base\n"
    b"class Actor:\n"
    b"    def __init__(self) -> None: ...\n"
    b"    @property\n"
    b"    def name(self) -> str: ...\n"
    b"    @property\n"
    b"    def production(self) -> Production: ...\n"
    b"    def make_effect(\n"
    b"        self,\n"
    b"        effect_type: type[_EffectT],\n"
    b"        *,\n"
    b"        effect_args: tuple[Any, ...],\n"
    b"        effect_kwargs: dict[str, Any],\n"
    b"    ) -> _EffectT: ...\n"
    b"    async def act(\n"
    b"        self,\n"
    b"        *,\n"
    b"        script: str,\n"
    b"        output_schema: dict[str, act_schema.FieldSpec],\n"
    b"    ) -> dict[str, _JsonValue]:\n"
    b'        """Return one validated JSON object from this Actor\'s persistent agent session."""\n'
    b"    async def cued(self, cue: Cue) -> tuple[Effect, ...]: ...\n"
    b"\n"
    b"@final\n"
    b"class ActorHandle:\n"
    b"    @property\n"
    b"    def name(self) -> str: ...\n"
    b"    async def cue(self, instruction: dict[Any, Any]) -> tuple[Effect, ...]: ...\n"
    b"\n"
    b"@final\n"
    b"class Cue:\n"
    b"    @property\n"
    b"    def id(self) -> str: ...\n"
    b"    @property\n"
    b"    def instruction(self) -> Mapping[Any, Any]: ...\n"
    b"    @property\n"
    b"    def source(self) -> str: ...\n"
    b"\n"
    b"class CueContextError(RuntimeError): ...\n"
    b"\n"
    b"@disjoint_base\n"
    b"class Effect:\n"
    b"    @property\n"
    b"    def id(self) -> str: ...\n"
    b"    @property\n"
    b"    def owner(self) -> str: ...\n"
    b"\n"
    b"class EffectContextError(RuntimeError): ...\n"
    b"\n"
    b"@disjoint_base\n"
    b"class Production:\n"
    b"    def __new__(cls, args: list[str], /) -> Production: ...\n"
    b"    def cast_actor(\n"
    b"        self,\n"
    b"        actor_type: type[Actor],\n"
    b"        *,\n"
    b"        name: str,\n"
    b"        agent_profile: AgentProfile,\n"
    b"        actor_args: tuple[Any, ...],\n"
    b"        actor_kwargs: dict[str, Any],\n"
    b"    ) -> ActorHandle: ...\n"
    b"    @overload\n"
    b"    def get_actor(self, name: str) -> ActorHandle | None: ...\n"
    b"    @overload\n"
    b"    def get_actor(self, pattern: Pattern[str]) -> list[ActorHandle]: ...\n"
    b"    def get_actors(self) -> list[ActorHandle]: ...\n"
    b"    async def start(self) -> None: ...\n"
    b"    async def scene(self) -> None: ...\n"
    b"    async def stop(self) -> None: ...\n"
    b"\n"
    b"__all__ = [\n"
    b'    "Actor",\n'
    b'    "ActorHandle",\n'
    b'    "AgentAuthenticationRequiredError",\n'
    b'    "AgentError",\n'
    b'    "AgentProfile",\n'
    b'    "AgentResultError",\n'
    b'    "AgentResultIssue",\n'
    b'    "AgentResultMissingError",\n'
    b'    "AgentSessionBrokenError",\n'
    b'    "AgentSessionBusyError",\n'
    b'    "AgentSessionError",\n'
    b'    "AgentSessionStartError",\n'
    b'    "AgentTurnError",\n'
    b'    "Cue",\n'
    b'    "CueContextError",\n'
    b'    "Effect",\n'
    b'    "EffectContextError",\n'
    b'    "Production",\n'
    b'    "act_schema",\n'
    b"]\n"
)
EXPECTED_ACT_SCHEMA_STUB = (PACKAGE / "act_schema.pyi").read_bytes()
EXPECTED_PY_TYPED = b""
EXPECTED_ENTRY_POINTS = b"[console_scripts]\ntroupe = troupe._runtime:main\n"
PUBLIC_EXPORTS = [
    "Actor",
    "ActorHandle",
    "AgentAuthenticationRequiredError",
    "AgentError",
    "AgentProfile",
    "AgentResultError",
    "AgentResultIssue",
    "AgentResultMissingError",
    "AgentSessionBrokenError",
    "AgentSessionBusyError",
    "AgentSessionError",
    "AgentSessionStartError",
    "AgentTurnError",
    "Cue",
    "CueContextError",
    "Effect",
    "EffectContextError",
    "Production",
    "act_schema",
]
PUBLIC_TYPE_EXPORTS = [name for name in PUBLIC_EXPORTS if name != "act_schema"]
SCHEMA_EXPORTS = [
    "BoolValue",
    "Field",
    "Float64Value",
    "Int64Value",
    "ListValue",
    "NullableValue",
    "ObjectValue",
    "SchemaCallbackError",
    "SchemaValue",
    "StrValue",
    "ValueRejected",
]
EXAMPLE_FILES = {
    "README.md": b"# Troupe examples\n",
    "actor_pipeline/__init__.py": b"",
    "actor_pipeline/production.py": b"import troupe\n",
    "cancellation_cleanup/__init__.py": b"",
    "cancellation_cleanup/production.py": b"import troupe\n",
    "cooperative_workers/__init__.py": b"",
    "cooperative_workers/production.py": b"import troupe\n",
    "hello_actor/__init__.py": b"",
    "hello_actor/production.py": b"import troupe\n",
    "live_agents/README.md": b"# Live agent examples\n",
    "live_agents/claude_actor/__init__.py": b"",
    "live_agents/claude_actor/production.py": b"import troupe\n",
    "live_agents/codex_actor/__init__.py": b"",
    "live_agents/codex_actor/production.py": b"import troupe\n",
    "live_agents/kimi_actor/__init__.py": b"",
    "live_agents/kimi_actor/production.py": b"import troupe\n",
    "live_agents/mixed_repository_repair/__init__.py": b"",
    "live_agents/mixed_repository_repair/production.py": b"import troupe\n",
    "repeating_scenes/__init__.py": b"",
    "repeating_scenes/production.py": b"import troupe\n",
}


def _is_module_path(name: str, root: str) -> bool:
    return name == root or name.startswith(f"{root}.")


def _dotted_name(node: ast.AST) -> str | None:
    if isinstance(node, ast.Name):
        return node.id
    if isinstance(node, ast.Attribute):
        owner = _dotted_name(node.value)
        return f"{owner}.{node.attr}" if owner is not None else None
    return None


def _toml(path: Path) -> dict[str, Any]:
    return tomllib.loads(path.read_text(encoding="utf-8"))


def _rust_without_comments(source: str) -> str:
    output: list[str] = []
    index = 0
    block_depth = 0

    while index < len(source):
        if block_depth:
            if source.startswith("/*", index):
                block_depth += 1
                output.extend("  ")
                index += 2
            elif source.startswith("*/", index):
                block_depth -= 1
                output.extend("  ")
                index += 2
            else:
                output.append("\n" if source[index] == "\n" else " ")
                index += 1
            continue

        if source.startswith("//", index):
            newline = source.find("\n", index + 2)
            if newline == -1:
                output.extend(" " * (len(source) - index))
                break
            output.extend(" " * (newline - index))
            output.append("\n")
            index = newline + 1
            continue

        if source.startswith("/*", index):
            block_depth = 1
            output.extend("  ")
            index += 2
            continue

        raw = re.match(r"(?:br|cr|r)(?P<hashes>#{0,255})\"", source[index:])
        if raw is not None:
            delimiter = '"' + raw.group("hashes")
            end = source.find(delimiter, index + raw.end())
            if end == -1:
                output.append(source[index:])
                break
            end += len(delimiter)
            output.append(source[index:end])
            index = end
            continue

        character = re.match(
            r"'(?:\\(?:[nrt0\\'\"]|x[0-9A-Fa-f]{2}|u\{[0-9A-Fa-f_]+\})|[^\\'\n])'",
            source[index:],
        )
        if character is not None:
            end = index + character.end()
            output.append(source[index:end])
            index = end
            continue

        if source[index] == '"':
            end = index + 1
            while end < len(source):
                if source[end] == "\\":
                    end += 2
                    continue
                if source[end] == '"':
                    end += 1
                    break
                end += 1
            output.append(source[index:end])
            index = end
            continue

        output.append(source[index])
        index += 1

    return "".join(output)


def _rust_without_test_modules(source: str) -> str:
    code = _rust_without_comments(source)
    structure = list(code)
    index = 0

    while index < len(code):
        raw = re.match(r"(?:br|cr|r)(?P<hashes>#{0,255})\"", code[index:])
        if raw is not None:
            delimiter = '"' + raw.group("hashes")
            end = code.find(delimiter, index + raw.end())
            end = len(code) if end == -1 else end + len(delimiter)
        else:
            character = re.match(
                r"'(?:\\(?:[nrt0\\'\"]|x[0-9A-Fa-f]{2}|u\{[0-9A-Fa-f_]+\})|[^\\'\n])'",
                code[index:],
            )
            if character is not None:
                end = index + character.end()
            elif code[index] == '"':
                end = index + 1
                while end < len(code):
                    if code[end] == "\\":
                        end += 2
                        continue
                    if code[end] == '"':
                        end += 1
                        break
                    end += 1
            else:
                index += 1
                continue

        for masked in range(index, min(end, len(code))):
            if structure[masked] != "\n":
                structure[masked] = " "
        index = end

    structure_text = "".join(structure)
    module = re.compile(
        r"#\s*\[\s*cfg\s*\(\s*test\s*\)\s*\]\s*mod\s+[A-Za-z_][A-Za-z0-9_]*\s*\{"
    )
    output = list(code)
    search_from = 0
    while match := module.search(structure_text, search_from):
        depth = 1
        end = match.end()
        while end < len(structure_text) and depth:
            if structure_text[end] == "{":
                depth += 1
            elif structure_text[end] == "}":
                depth -= 1
            end += 1
        if depth:
            raise ValueError("unbalanced #[cfg(test)] module")
        for removed in range(match.start(), end):
            if output[removed] != "\n":
                output[removed] = " "
        search_from = end

    return "".join(output)


def test_rust_comment_stripper_preserves_literals_and_removes_comments() -> None:
    source = """\
// PyModule::import(py, "contextvars");
/* outer /* struct CuedScope; */ comment */
let ordinary = "/* not a comment */";
let raw = r#"// not a comment"#;
let character = '/';
let lifetime: Python<'_>;
PyModule::import(py, "contextvars");
let context_name = c"contextvars";
struct CuedScope;
"""

    code = _rust_without_comments(source)

    assert code.count('PyModule::import(py, "contextvars");') == 1
    assert '"/* not a comment */"' in code
    assert 'r#"// not a comment"#' in code
    assert "'/'" in code
    assert "Python<'_>" in code
    assert 'c"contextvars"' in code
    assert code.count("struct CuedScope;") == 1


def test_rust_runtime_filter_removes_only_cfg_test_modules() -> None:
    source = '''\
let runtime_name = "ContextVar";
#[cfg(test)]
mod tests {
    use std::sync::mpsc;
    type Worker = JoinHandle<()>;
    const TEST_NAME: &str = "ContextVar";
    mod nested { const VALUE: &str = "}"; }
}
let runtime_after = c"contextvars";
'''

    code = _rust_without_test_modules(source)

    assert code.count('"ContextVar"') == 1
    assert "TEST_NAME" not in code
    assert "mpsc" not in code
    assert "JoinHandle" not in code
    assert "mod nested" not in code
    assert 'c"contextvars"' in code
    with pytest.raises(ValueError, match=r"^unbalanced #\[cfg\(test\)\] module$"):
        _rust_without_test_modules("#[cfg(test)] mod tests {")


def _verifier() -> ModuleType:
    script = ROOT / "scripts" / "verify_wheel.py"
    spec = importlib.util.spec_from_file_location("_troupe_verify_wheel_test", script)
    assert spec is not None
    assert spec.loader is not None
    module = importlib.util.module_from_spec(spec)
    sys.modules[spec.name] = module
    spec.loader.exec_module(module)
    return module


def test_default_wheel_smoke_rejects_every_agent_test_support_surface() -> None:
    smoke = _verifier().SMOKE
    for name in (
        "_agent_launch_specs_for_test",
        "_agent_test_set_launch",
        "_agent_test_reset_launch",
        "_agent_test_hold_opening",
        "_agent_test_release_opening",
        "_agent_test_hold_opening_backoff",
        "_agent_test_release_opening_backoff",
        "_agent_test_opening_backoff_state",
        "_agent_test_hold_configuration_ready",
        "_agent_test_release_configuration_ready",
        "_agent_test_hold_mcp_ready",
        "_agent_test_release_mcp_ready",
        "_agent_test_readiness_gate_states",
        "_agent_test_hold_turn_registration",
        "_agent_test_release_turn_registration",
        "_agent_test_hold_turn_intake",
        "_agent_test_release_turn_intake",
        "_agent_test_hold_turn_submission",
        "_agent_test_release_turn_submission",
        "_agent_test_hold_turn_response_flush",
        "_agent_test_release_turn_response_flush",
        "_agent_test_hold_turn_settlement",
        "_agent_test_release_turn_settlement",
        "_agent_test_hold_turn_terminal_delivery",
        "_agent_test_release_turn_terminal_delivery",
        "_agent_test_hold_turn_outcome",
        "_agent_test_release_turn_outcome",
        "_agent_test_turn_gate_states",
        "_agent_test_result_generation_isolation",
        "_agent_state_for_test",
        "_agent_has_queued_turn_for_test",
        "_agent_fail_transport_for_test",
        "_agent_ready_for_test",
        "_agent_shutdown_for_test",
        "_agent_is_shutting_down_for_test",
        "_agent_fail_result_listener_for_test",
    ):
        assert smoke.count(f'"{name}"') == 1


def _add_tar_bytes(archive: tarfile.TarFile, name: str, data: bytes) -> None:
    info = tarfile.TarInfo(name)
    info.size = len(data)
    archive.addfile(info, io.BytesIO(data))


def _expanded_tags(tag: str) -> list[str]:
    python_tag, abi_tag, platform_tag = tag.split("-", maxsplit=2)
    return [
        f"{python}-{abi}-{platform}"
        for python in python_tag.split(".")
        for abi in abi_tag.split(".")
        for platform in platform_tag.split(".")
    ]


def _record_bytes(
    files: dict[str, bytes],
    record_name: str,
    mutation: str | None,
) -> bytes:
    stream = io.StringIO()
    writer = csv.writer(stream, lineterminator="\n")
    rows: list[list[str]] = []
    for name, data in files.items():
        digest = base64.urlsafe_b64encode(hashlib.sha256(data).digest()).rstrip(b"=")
        rows.append([name, f"sha256={digest.decode('ascii')}", str(len(data))])

    native_index = next(
        (index for index, row in enumerate(rows) if row[0].endswith(".so")),
        0,
    )
    if mutation == "wrong-size":
        rows[native_index][2] = str(int(rows[native_index][2]) + 1)
    elif mutation == "wrong-hash":
        rows[native_index][1] = "sha256=" + ("A" * 43)
    elif mutation == "phantom":
        rows.append(["phantom.py", "sha256=" + ("A" * 43), "0"])
    elif mutation == "duplicate-row":
        rows.append(rows[0].copy())
    elif mutation == "missing-member-row":
        rows.pop(native_index)
    elif mutation == "extra-column":
        rows[0].append("extra")
    elif mutation == "invalid-size":
        rows[0][2] = "not-a-size"
    elif mutation == "negative-size":
        rows[0][2] = "-1"
    elif mutation == "empty-hash":
        rows[0][1] = ""
    elif mutation == "empty-size":
        rows[0][2] = ""

    writer.writerows(rows)
    if mutation != "missing-self-row":
        if mutation == "self-nonempty":
            writer.writerow([record_name, "sha256=" + ("A" * 43), "1"])
        else:
            writer.writerow([record_name, "", ""])
    return stream.getvalue().encode("utf-8")


def _synthetic_artifacts(
    tmp_path: Path,
    *,
    extra_source_python: bool = False,
    extra_source_stub: bool = False,
    extra_sdist_python: bool = False,
    extra_sdist_stub: bool = False,
    extra_wheel_python: bool = False,
    extra_package_python: bool = False,
    source_wrapper: bytes = EXPECTED_WRAPPER,
    source_stub: bytes | None = EXPECTED_STUB,
    source_act_schema_stub: bytes | None = EXPECTED_ACT_SCHEMA_STUB,
    source_py_typed: bytes | None = EXPECTED_PY_TYPED,
    sdist_wrapper: bytes = EXPECTED_WRAPPER,
    sdist_stub: bytes | None = EXPECTED_STUB,
    sdist_act_schema_stub: bytes | None = EXPECTED_ACT_SCHEMA_STUB,
    sdist_py_typed: bytes | None = EXPECTED_PY_TYPED,
    missing_source_example: str | None = None,
    extra_source_example: bool = False,
    missing_sdist_example: str | None = None,
    changed_sdist_example: str | None = None,
    extra_sdist_example: bool = False,
    missing_sdist_rust: str | None = None,
    changed_sdist_rust: str | None = None,
    sdist_unsafe: str | None = None,
    wheel_wrapper: bytes = EXPECTED_WRAPPER,
    wheel_stub: bytes | None = EXPECTED_STUB,
    wheel_act_schema_stub: bytes | None = EXPECTED_ACT_SCHEMA_STUB,
    wheel_py_typed: bytes | None = EXPECTED_PY_TYPED,
    native_count: int = 1,
    runtime_stem: str = "_runtime",
    extra_native: str | None = None,
    tag: str = "cp310-abi3-manylinux_2_17_x86_64",
    wheel_tags: list[str] | None = None,
    requires_dist: bool = False,
    metadata_name: str = "troupe",
    metadata_version: str = "0.1.0",
    requires_python: str = ">=3.10",
    wheel_version: str | None = "1.0",
    root_is_purelib: str = "false",
    entry_points: str = "valid",
    forbidden_file: str | None = None,
    record_mutation: str | None = None,
    duplicate_member: bool = False,
) -> tuple[Path, Path, Path]:
    source = tmp_path / "source" / "troupe"
    source.mkdir(parents=True)
    (source / "__init__.py").write_bytes(source_wrapper)
    if source_stub is not None:
        (source / "__init__.pyi").write_bytes(source_stub)
    if source_act_schema_stub is not None:
        (source / "act_schema.pyi").write_bytes(source_act_schema_stub)
    if source_py_typed is not None:
        (source / "py.typed").write_bytes(source_py_typed)
    if extra_source_python:
        (source / "nested").mkdir()
        (source / "nested" / "helper.py").write_text("VALUE = 1\n", encoding="utf-8")
    if extra_source_stub:
        (source / "helper.pyi").write_text("VALUE: int\n", encoding="utf-8")

    for name, data in SYNTHETIC_RUST_BUILD_INPUTS.items():
        path = tmp_path / name
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_bytes(data)

    source_examples = tmp_path / "examples"
    for name, data in EXAMPLE_FILES.items():
        if name == missing_source_example:
            continue
        path = source_examples / name
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_bytes(data)
    if extra_source_example:
        (source_examples / "unexpected.txt").write_text(
            "unexpected\n",
            encoding="utf-8",
        )

    sdist = tmp_path / "troupe-0.1.0.tar.gz"
    sdist_root = "troupe-0.1.0"
    sdist_package = f"{sdist_root}/src/troupe"
    with tarfile.open(sdist, "w:gz") as archive:
        _add_tar_bytes(archive, f"{sdist_package}/__init__.py", sdist_wrapper)
        if sdist_stub is not None:
            _add_tar_bytes(archive, f"{sdist_package}/__init__.pyi", sdist_stub)
        if sdist_act_schema_stub is not None:
            _add_tar_bytes(
                archive,
                f"{sdist_package}/act_schema.pyi",
                sdist_act_schema_stub,
            )
        if sdist_py_typed is not None:
            _add_tar_bytes(archive, f"{sdist_package}/py.typed", sdist_py_typed)
        if extra_sdist_python:
            _add_tar_bytes(
                archive,
                f"{sdist_package}/nested/helper.py",
                b"VALUE = 1\n",
            )
        if extra_sdist_stub:
            _add_tar_bytes(archive, f"{sdist_package}/helper.pyi", b"VALUE: int\n")
        for name, data in EXAMPLE_FILES.items():
            if name == missing_sdist_example:
                continue
            if name == changed_sdist_example:
                data = b"changed\n"
            _add_tar_bytes(archive, f"{sdist_root}/examples/{name}", data)
        if extra_sdist_example:
            _add_tar_bytes(
                archive,
                f"{sdist_root}/examples/unexpected.txt",
                b"unexpected\n",
            )
        for name, data in SYNTHETIC_RUST_BUILD_INPUTS.items():
            if name == missing_sdist_rust:
                continue
            if name == changed_sdist_rust:
                data = b"changed\n"
            _add_tar_bytes(archive, f"{sdist_root}/{name}", data)
        if sdist_unsafe == "traversal":
            _add_tar_bytes(archive, "../escape.dat", b"unsafe\n")
        elif sdist_unsafe == "absolute":
            _add_tar_bytes(archive, "/escape.dat", b"unsafe\n")
        elif sdist_unsafe == "symlink":
            link = tarfile.TarInfo(f"{sdist_package}/linked.dat")
            link.type = tarfile.SYMTYPE
            link.linkname = "/tmp/escape.dat"
            archive.addfile(link)

    wheel = tmp_path / f"troupe-0.1.0-{tag}.whl"
    metadata = (
        "Metadata-Version: 2.4\n"
        f"Name: {metadata_name}\n"
        f"Version: {metadata_version}\n"
        f"Requires-Python: {requires_python}\n"
    )
    if requires_dist:
        metadata += "Requires-Dist: typing-extensions\n"
    resolved_wheel_tags = wheel_tags if wheel_tags is not None else _expanded_tags(tag)
    wheel_metadata = ""
    if wheel_version is not None:
        wheel_metadata += f"Wheel-Version: {wheel_version}\n"
    wheel_metadata += (
        "Generator: test\n"
        f"Root-Is-Purelib: {root_is_purelib}\n"
        + "".join(f"Tag: {wheel_tag}\n" for wheel_tag in resolved_wheel_tags)
    )
    dist_info = "troupe-0.1.0.dist-info"
    wheel_files: dict[str, bytes] = {
        "troupe/__init__.py": wheel_wrapper,
        f"{dist_info}/METADATA": metadata.encode("utf-8"),
        f"{dist_info}/WHEEL": wheel_metadata.encode("utf-8"),
    }
    if wheel_stub is not None:
        wheel_files["troupe/__init__.pyi"] = wheel_stub
    if wheel_act_schema_stub is not None:
        wheel_files["troupe/act_schema.pyi"] = wheel_act_schema_stub
    if wheel_py_typed is not None:
        wheel_files["troupe/py.typed"] = wheel_py_typed
    if entry_points == "valid":
        wheel_files[f"{dist_info}/entry_points.txt"] = EXPECTED_ENTRY_POINTS
    elif entry_points == "compact":
        wheel_files[f"{dist_info}/entry_points.txt"] = (
            b"[console_scripts]\ntroupe=troupe._runtime:main\n"
        )
    elif entry_points == "wrong":
        wheel_files[f"{dist_info}/entry_points.txt"] = (
            b"[console_scripts]\ntroupe = troupe:main\n"
        )
    elif entry_points == "extra":
        wheel_files[f"{dist_info}/entry_points.txt"] = (
            EXPECTED_ENTRY_POINTS + b"other = troupe._runtime:main\n"
        )
    elif entry_points == "duplicate-key":
        wheel_files[f"{dist_info}/entry_points.txt"] = (
            EXPECTED_ENTRY_POINTS + b"troupe = troupe._runtime:main\n"
        )
    elif entry_points == "duplicate-section":
        wheel_files[f"{dist_info}/entry_points.txt"] = (
            EXPECTED_ENTRY_POINTS
            + b"[console_scripts]\ntroupe = troupe._runtime:main\n"
        )
    elif entry_points == "extra-section":
        wheel_files[f"{dist_info}/entry_points.txt"] = (
            EXPECTED_ENTRY_POINTS + b"[other]\nvalue = target\n"
        )
    elif entry_points == "wrong-section":
        wheel_files[f"{dist_info}/entry_points.txt"] = (
            b"[other]\ntroupe = troupe._runtime:main\n"
        )
    elif entry_points == "default-section":
        wheel_files[f"{dist_info}/entry_points.txt"] = (
            b"[DEFAULT]\ntroupe = troupe._runtime:main\n[console_scripts]\n"
        )
    elif entry_points == "case-key":
        wheel_files[f"{dist_info}/entry_points.txt"] = (
            b"[console_scripts]\nTROUPE = troupe._runtime:main\n"
        )
    elif entry_points == "colon-delimiter":
        wheel_files[f"{dist_info}/entry_points.txt"] = (
            b"[console_scripts]\ntroupe: troupe._runtime:main\n"
        )
    elif entry_points == "malformed":
        wheel_files[f"{dist_info}/entry_points.txt"] = b"troupe=troupe._runtime:main\n"
    elif entry_points == "invalid-utf8":
        wheel_files[f"{dist_info}/entry_points.txt"] = b"\xff"
    elif entry_points != "missing":
        raise AssertionError(f"unknown entry_points fixture: {entry_points}")
    for index in range(native_count):
        suffix = "" if index == 0 else f".extra{index}"
        wheel_files[f"troupe/{runtime_stem}{suffix}.abi3.so"] = b"native"
    if extra_native is not None:
        wheel_files[f"troupe/{extra_native}"] = b"native"
    if extra_wheel_python:
        wheel_files["troupe/nested/helper.py"] = b"VALUE = 1\n"
    if extra_package_python:
        wheel_files["other_package/__init__.py"] = b"VALUE = 1\n"
    if forbidden_file is not None:
        wheel_files[forbidden_file] = b"forbidden"

    record_name = f"{dist_info}/RECORD"
    record = _record_bytes(wheel_files, record_name, record_mutation)
    with zipfile.ZipFile(wheel, "w") as archive:
        for name, data in wheel_files.items():
            archive.writestr(name, data)
        if record_mutation != "missing-record":
            archive.writestr(record_name, record)
        if duplicate_member:
            with warnings.catch_warnings():
                warnings.simplefilter("ignore", UserWarning)
                archive.writestr("troupe/__init__.py", wheel_wrapper)

    return source, sdist, wheel


def test_runtime_package_has_exact_thin_sources() -> None:
    python_files = sorted(path.relative_to(PACKAGE).as_posix() for path in PACKAGE.rglob("*.py"))
    stub_files = sorted(path.relative_to(PACKAGE).as_posix() for path in PACKAGE.rglob("*.pyi"))

    assert python_files == ["__init__.py"]
    assert stub_files == ["__init__.pyi", "act_schema.pyi", "diagnostics.pyi"]
    assert "from ._runtime import diagnostics as diagnostics" in (
        PACKAGE / "__init__.py"
    ).read_text(encoding="utf-8")
    assert "from . import diagnostics as diagnostics" in (
        PACKAGE / "__init__.pyi"
    ).read_text(encoding="utf-8")
    assert (PACKAGE / "py.typed").read_bytes() == EXPECTED_PY_TYPED


def test_python_project_metadata_and_build_configuration() -> None:
    config = _toml(ROOT / "pyproject.toml")

    assert config["build-system"] == {
        "requires": ["maturin==1.14.1"],
        "build-backend": "maturin",
    }

    project = config["project"]
    assert project["name"] == "troupe"
    assert project["requires-python"] == ">=3.10"
    assert project.get("dependencies", []) == []
    assert project["scripts"] == {"troupe": "troupe._runtime:main"}
    classifiers = set(project["classifiers"])
    assert "Programming Language :: Python :: 3 :: Only" in classifiers
    assert "Programming Language :: Python :: Implementation :: CPython" in classifiers
    assert all("PyPy" not in classifier for classifier in classifiers)
    assert all("Free Threading" not in classifier for classifier in classifiers)

    maturin = config["tool"]["maturin"]
    assert maturin == {
        "python-source": "src",
        "manifest-path": "rust/Cargo.toml",
        "module-name": "troupe._runtime",
        "locked": True,
        "include": [{"path": "examples/**/*", "format": "sdist"}],
        "exclude": ["**/__pycache__/**", "**/*.pyc", "**/*.pyo"],
        "sbom": {"rust": False},
    }
    assert "mypy" not in config["tool"]

    development = config["dependency-groups"]["dev"]
    assert set(development) == {
        "maturin==1.14.1",
        "mypy>=1.18.1",
        "PyYAML>=6.0.3",
        "pytest",
        "tomli>=1.1.0; python_version < '3.11'",
        "typing-extensions>=4.15.0",
        "wheel>=0.45.1",
    }

    locked_packages = {package["name"] for package in _toml(ROOT / "uv.lock")["package"]}
    assert {"pyyaml", "wheel"} <= locked_packages
    repository_wheels = subprocess.run(
        [
            "git",
            "ls-files",
            "--cached",
            "--others",
            "--exclude-standard",
            "--",
            "*.whl",
            ":(glob)**/*.whl",
        ],
        cwd=ROOT,
        check=True,
        capture_output=True,
        text=True,
    )
    assert repository_wheels.stdout == ""

    cache_files = {
        entry["file"]
        for entry in config["tool"]["uv"]["cache-keys"]
        if "file" in entry
    }
    assert {
        "pyproject.toml",
        "README.md",
        "rust/Cargo.toml",
        "rust/Cargo.lock",
        "rust/crates/**/Cargo.toml",
        "rust/crates/**/*.rs",
        "rust/src/**/*.rs",
        "src/troupe/**/*.py",
        "src/troupe/**/*.pyi",
        "src/troupe/py.typed",
    } <= cache_files


def test_rust_manifest_and_source_boundary() -> None:
    config = _toml(ROOT / "rust" / "Cargo.toml")
    agent_config = _toml(
        ROOT / "rust" / "crates" / "troupe-agent-runtime" / "Cargo.toml"
    )

    assert config["workspace"] == {
        "members": [
            "crates/troupe-agent-runtime",
            "crates/troupe-diagnostics-core",
            "crates/troupe-diagnostics-runtime",
            "crates/troupe-diagnostics-perfetto",
        ],
        "resolver": "3",
    }
    assert config["lib"]["name"] == "_runtime"
    assert config["lib"]["crate-type"] == ["cdylib"]
    assert config["features"] == {
        "default": [],
        "agent-test-support": ["troupe-agent-runtime/agent-test-support"],
        "diagnostics-test-support": [],
    }

    dependencies = config["dependencies"]
    assert set(dependencies) == {
        "clap",
        "pyo3",
        "pyo3-async-runtimes",
        "reqwest",
        "serde",
        "serde_json",
        "tokio",
        "tokio-util",
        "troupe-agent-runtime",
        "troupe-diagnostics-core",
        "troupe-diagnostics-perfetto",
        "troupe-diagnostics-runtime",
        "uuid",
    }
    assert dependencies["pyo3"] == {
        "version": "0.29.0",
        "features": ["abi3-py310", "experimental-async"],
    }
    assert dependencies["pyo3-async-runtimes"] == {
        "version": "0.29.0",
        "features": ["tokio-runtime"],
    }
    assert dependencies["tokio"] == {
        "version": "1",
        "features": ["macros", "rt-multi-thread", "sync"],
    }
    assert dependencies["tokio-util"] == {
        "version": "0.7",
        "features": ["rt"],
    }
    assert dependencies["clap"] == {
        "version": "4",
        "features": ["derive"],
    }
    assert dependencies["uuid"] == {
        "version": "1",
        "features": ["v4"],
    }
    assert dependencies["troupe-agent-runtime"] == {
        "path": "crates/troupe-agent-runtime"
    }
    for crate in (
        "troupe-diagnostics-core",
        "troupe-diagnostics-perfetto",
        "troupe-diagnostics-runtime",
    ):
        assert dependencies[crate] == {"path": f"crates/{crate}"}
    assert "extension-module" not in dependencies["pyo3"]["features"]

    assert agent_config["package"]["name"] == "troupe-agent-runtime"
    assert agent_config["package"]["publish"] is False
    assert agent_config["features"] == {"default": [], "agent-test-support": []}
    agent_dependencies = agent_config["dependencies"]
    assert set(agent_dependencies) == {
        "agent-client-protocol",
        "base64",
        "bytes",
        "futures",
        "getrandom",
        "http-body-util",
        "hyper",
        "hyper-util",
        "libc",
        "pyo3",
        "pyo3-async-runtimes",
        "serde_json",
        "tokio",
        "tokio-util",
        "troupe-diagnostics-core",
        "uuid",
    }
    assert agent_dependencies["agent-client-protocol"] == {
        "version": "=2.0.0",
        "features": ["unstable_end_turn_token_usage"],
    }
    assert agent_dependencies["troupe-diagnostics-core"] == {
        "path": "../troupe-diagnostics-core"
    }
    assert agent_dependencies["hyper"] == {
        "version": "1",
        "features": ["http1", "server"],
    }
    assert agent_dependencies["serde_json"] == {
        "version": "1",
        "features": ["arbitrary_precision"],
    }
    assert agent_dependencies["tokio"] == {
        "version": "1",
        "features": [
            "io-util",
            "macros",
            "net",
            "process",
            "rt-multi-thread",
            "sync",
            "time",
        ],
    }

    rust_sources = sorted(
        path.relative_to(ROOT / "rust").as_posix()
        for path in (ROOT / "rust").rglob("*.rs")
        if "target" not in path.parts
    )
    assert {
        "src/lib.rs",
        "crates/troupe-agent-runtime/src/lib.rs",
        "crates/troupe-diagnostics-core/src/lib.rs",
        "crates/troupe-diagnostics-runtime/src/lib.rs",
        "crates/troupe-diagnostics-perfetto/src/lib.rs",
    } <= set(rust_sources)
    source_code = "\n".join(
        _rust_without_test_modules(
            (ROOT / "rust" / name).read_text(encoding="utf-8")
        )
        for name in rust_sources
    )
    assert '"contextvars"' not in source_code
    assert '"ContextVar"' not in source_code
    effect_source = (ROOT / "rust" / "src" / "orchestration" / "effect.rs").read_text(
        encoding="utf-8"
    )
    assert "struct CuedScope" not in _rust_without_test_modules(effect_source)
    mailbox_source = (ROOT / "rust" / "src" / "orchestration" / "mailbox.rs").read_text(
        encoding="utf-8"
    )
    mailbox_code = _rust_without_test_modules(mailbox_source)
    for forbidden_syntax in (
        r"\bmpsc::",
        r"\bSemaphore::",
        r"\bJoinHandle\s*<",
        r"^\s*use\s+[^;]*\bmpsc\b",
        r"^\s*use\s+[^;]*\bSemaphore\b",
        r"^\s*use\s+[^;]*\bJoinHandle\b",
    ):
        assert re.search(forbidden_syntax, mailbox_code, re.MULTILINE) is None
    locked_packages = {
        package["name"]
        for package in _toml(ROOT / "rust" / "Cargo.lock")["package"]
    }
    assert "uuid" in locked_packages
    invocation_source = (
        ROOT / "rust" / "src" / "application" / "invocation.rs"
    ).read_text(encoding="utf-8")
    argument_source = (
        ROOT / "rust" / "src" / "application" / "diagnostic_cli" / "args.rs"
    ).read_text(encoding="utf-8")
    assert "#[derive(Clone, Debug, Parser, Eq, PartialEq)]" in argument_source
    assert "TroupeArgs::command()" in invocation_source
    assert ".try_get_matches_from" in invocation_source
    assert "TroupeArgs::from_arg_matches" in invocation_source
    assert "#[pymodule(gil_used = true)]" in (
        ROOT / "rust" / "src" / "lib.rs"
    ).read_text(encoding="utf-8")
    for private_name in ("_Runtime", "PhaseFailure", "ProductionFailed"):
        for path in (
            ROOT / "src" / "troupe" / "__init__.py",
            ROOT / "src" / "troupe" / "__init__.pyi",
            ROOT / "README.md",
        ):
            assert private_name not in path.read_text(encoding="utf-8")


def test_installed_console_entry_point_targets_the_native_main() -> None:
    entries = [
        entry
        for entry in importlib.metadata.entry_points(group="console_scripts")
        if entry.name == "troupe"
    ]

    assert len(entries) == 1
    assert entries[0].value == "troupe._runtime:main"


def test_lockfiles_exist_and_are_nonempty() -> None:
    assert (ROOT / "uv.lock").stat().st_size > 0
    assert (ROOT / "rust" / "Cargo.lock").stat().st_size > 0


def test_verifier_accepts_the_exact_synthetic_layout(tmp_path: Path) -> None:
    verifier = _verifier()
    source, sdist, wheel = _synthetic_artifacts(tmp_path)

    verifier._validate_artifacts(source, sdist, wheel)


def test_verifier_accepts_pinned_maturin_entry_point_format(tmp_path: Path) -> None:
    verifier = _verifier()
    source, sdist, wheel = _synthetic_artifacts(tmp_path, entry_points="compact")

    verifier._validate_artifacts(source, sdist, wheel)


@pytest.mark.parametrize(
    "changes",
    [
        {"extra_source_python": True},
        {"extra_source_stub": True},
        {"extra_sdist_python": True},
        {"extra_sdist_stub": True},
        {"missing_source_example": "README.md"},
        {"extra_source_example": True},
        {"changed_sdist_example": "actor_pipeline/production.py"},
        {"extra_sdist_example": True},
        {"missing_sdist_rust": "rust/crates/troupe-agent-runtime/Cargo.toml"},
        {"changed_sdist_rust": "rust/crates/troupe-agent-runtime/src/lib.rs"},
        {"extra_wheel_python": True},
        {"extra_package_python": True},
        {"forbidden_file": "helper.py"},
        {"forbidden_file": "helper.pyi"},
        {"forbidden_file": "other_package/helper.pyi"},
        {"forbidden_file": "other_package/native.so"},
        {"source_stub": None},
        {"source_act_schema_stub": None},
        {"source_act_schema_stub": b"wrong schema stub\n"},
        {"source_py_typed": None},
        {"source_py_typed": b"partial\n"},
        {"sdist_stub": None},
        {"sdist_stub": b"wrong stub\n"},
        {"sdist_act_schema_stub": None},
        {"sdist_act_schema_stub": b"wrong schema stub\n"},
        {"sdist_py_typed": None},
        {"sdist_py_typed": b"partial\n"},
        {"wheel_stub": None},
        {"wheel_act_schema_stub": None},
        {"wheel_act_schema_stub": b"wrong schema stub\n"},
        {"wheel_py_typed": None},
        {"wheel_py_typed": b"partial\n"},
        {"source_wrapper": b"wrong wrapper\n"},
        {"sdist_wrapper": b"wrong wrapper\n"},
        {"wheel_wrapper": b"wrong wrapper\n"},
        {"wheel_stub": b"wrong stub\n"},
        {"native_count": 2},
        {"runtime_stem": "_runtime_helper"},
        {"extra_native": "_runtime.so"},
        {"extra_native": "other.so"},
        {"tag": "cp311-abi3-manylinux_2_17_x86_64"},
        {"tag": "cp310-abi3-linux_x86_64"},
        {"tag": "cp310-abi3-musllinux_1_2_x86_64"},
        {"tag": "cp310-abi3-manylinux_2_17_aarch64"},
        {"tag": "cp310-abi3-macosx_10_15_x86_64"},
        {"tag": "cp310-abi3-win_amd64"},
        {"tag": "cp313t-cp313t-manylinux_2_17_x86_64"},
        {"tag": "cp310-abi3-manylinux_2_17_x86_64.win_amd64"},
        {"tag": "cp310.cp311-abi3-manylinux_2_17_x86_64"},
        {"requires_dist": True},
        {"metadata_name": "other"},
        {"metadata_version": "9.9.9"},
        {"requires_python": ">=3.11"},
        {"wheel_version": None},
        {"wheel_version": "2.0"},
        {"root_is_purelib": "true"},
        {"entry_points": "missing"},
        {"entry_points": "wrong"},
        {"entry_points": "extra"},
        {"entry_points": "duplicate-key"},
        {"entry_points": "duplicate-section"},
        {"entry_points": "extra-section"},
        {"entry_points": "wrong-section"},
        {"entry_points": "default-section"},
        {"entry_points": "case-key"},
        {"entry_points": "colon-delimiter"},
        {"entry_points": "malformed"},
        {"entry_points": "invalid-utf8"},
        {
            "wheel_tags": ["cp310-abi3-manylinux2014_x86_64"],
        },
        {"wheel_tags": []},
        {
            "wheel_tags": [
                "cp310-abi3-manylinux_2_17_x86_64",
                "cp310-abi3-manylinux_2_17_x86_64",
            ],
        },
        {
            "tag": "cp310-abi3-manylinux_2_17_x86_64.manylinux2014_x86_64",
            "wheel_tags": ["cp310-abi3-manylinux_2_17_x86_64"],
        },
        {
            "wheel_tags": [
                "cp310-abi3-manylinux_2_17_x86_64",
                "cp310-abi3-manylinux2014_x86_64",
            ],
        },
        {"forbidden_file": "troupe/__pycache__/cached.pyc"},
        {"forbidden_file": "troupe/cache.pyc"},
        {"forbidden_file": "troupe/cache.pyo"},
        {"forbidden_file": "tests/test_runtime.py"},
        {"record_mutation": "wrong-size"},
        {"record_mutation": "wrong-hash"},
        {"record_mutation": "phantom"},
        {"record_mutation": "duplicate-row"},
        {"record_mutation": "missing-member-row"},
        {"record_mutation": "extra-column"},
        {"record_mutation": "invalid-size"},
        {"record_mutation": "negative-size"},
        {"record_mutation": "empty-hash"},
        {"record_mutation": "empty-size"},
        {"record_mutation": "missing-self-row"},
        {"record_mutation": "self-nonempty"},
        {"record_mutation": "missing-record"},
        {"forbidden_file": "/escape.dat"},
        {"forbidden_file": "../escape.dat"},
        {"sdist_unsafe": "traversal"},
        {"sdist_unsafe": "absolute"},
        {"sdist_unsafe": "symlink"},
        {"duplicate_member": True},
    ],
)
def test_verifier_rejects_invalid_artifacts(
    tmp_path: Path, changes: dict[str, object]
) -> None:
    verifier = _verifier()
    source, sdist, wheel = _synthetic_artifacts(tmp_path, **changes)

    with pytest.raises(verifier.VerificationError):
        verifier._validate_artifacts(source, sdist, wheel)


@pytest.mark.parametrize("missing_sdist_example", EXAMPLE_FILES)
def test_verifier_rejects_each_missing_sdist_example(
    tmp_path: Path,
    missing_sdist_example: str,
) -> None:
    verifier = _verifier()
    source, sdist, wheel = _synthetic_artifacts(
        tmp_path,
        missing_sdist_example=missing_sdist_example,
    )

    with pytest.raises(verifier.VerificationError):
        verifier._validate_artifacts(source, sdist, wheel)


@pytest.mark.parametrize("reverse_wheel_tags", [False, True], ids=["forward", "reverse"])
def test_verifier_accepts_compressed_equivalent_manylinux_tags(
    tmp_path: Path,
    reverse_wheel_tags: bool,
) -> None:
    verifier = _verifier()
    tag = "cp310-abi3-manylinux_2_17_x86_64.manylinux2014_x86_64"
    wheel_tags = _expanded_tags(tag)
    if reverse_wheel_tags:
        wheel_tags.reverse()
    source, sdist, wheel = _synthetic_artifacts(
        tmp_path,
        tag=tag,
        wheel_tags=wheel_tags,
    )

    verifier._validate_artifacts(source, sdist, wheel)


@pytest.mark.parametrize(
    "tag",
    [
        "cp310-abi3-manylinux2014_x86_64",
        "cp310-abi3-manylinux_2_34_x86_64",
    ],
)
def test_verifier_accepts_supported_alias_and_newer_local_manylinux_tags(
    tmp_path: Path,
    tag: str,
) -> None:
    verifier = _verifier()
    source, sdist, wheel = _synthetic_artifacts(tmp_path, tag=tag)

    verifier._validate_artifacts(source, sdist, wheel)


def test_requested_manylinux_must_be_present_in_the_wheel_tag(tmp_path: Path) -> None:
    verifier = _verifier()
    source, _, newer = _synthetic_artifacts(
        tmp_path / "newer",
        tag="cp310-abi3-manylinux_2_34_x86_64",
    )
    with pytest.raises(verifier.VerificationError):
        verifier._validate_wheel(source, newer, required_manylinux="2_17")

    for name, tag in (
        ("numeric", "cp310-abi3-manylinux_2_17_x86_64"),
        ("alias", "cp310-abi3-manylinux2014_x86_64"),
    ):
        accepted_source, _, accepted = _synthetic_artifacts(
            tmp_path / name,
            tag=tag,
        )
        verifier._validate_wheel(
            accepted_source,
            accepted,
            required_manylinux="2_17",
        )


def test_verifier_build_command_and_environment_are_isolated(tmp_path: Path) -> None:
    verifier = _verifier()
    output = tmp_path / "artifacts"
    base = [
        "maturin",
        "build",
        "--sdist",
        "--locked",
        "--manifest-path",
        "rust/Cargo.toml",
        "--out",
        str(output),
    ]

    assert verifier._maturin_command(
        output,
        release=False,
        target=None,
        manylinux=None,
    ) == base
    assert verifier._maturin_command(
        output,
        release=True,
        target="x86_64-unknown-linux-gnu",
        manylinux="2_17",
    ) == [
        *base,
        "--release",
        "--target",
        "x86_64-unknown-linux-gnu",
        "--manylinux",
        "2_17",
    ]

    original = {
        "CONDA_PREFIX": "/conda",
        "PYTHONPATH": "/source",
        "PYTHONHOME": "/python",
        "VIRTUAL_ENV": "/outer-venv",
        "PYTHONDONTWRITEBYTECODE": "0",
        "PATH": os.environ.get("PATH", ""),
        "KEEP": "value",
    }
    build = verifier._build_environment(original)
    assert "CONDA_PREFIX" not in build
    assert build["PYTHONPATH"] == "/source"
    assert build["KEEP"] == "value"

    smoke = verifier._smoke_environment(original)
    assert {
        "CONDA_PREFIX",
        "PYTHONPATH",
        "PYTHONHOME",
        "VIRTUAL_ENV",
    }.isdisjoint(smoke)
    assert smoke["PYTHONDONTWRITEBYTECODE"] == "1"
    assert smoke["KEEP"] == "value"
    assert original["CONDA_PREFIX"] == "/conda"
    assert original["PYTHONDONTWRITEBYTECODE"] == "0"



def test_smoke_environment_injects_bytecode_guard_when_caller_omits_it() -> None:
    verifier = _verifier()
    original = {"KEEP": "value"}

    smoke = verifier._smoke_environment(original)

    assert smoke["PYTHONDONTWRITEBYTECODE"] == "1"
    assert smoke["KEEP"] == "value"
    assert "PYTHONDONTWRITEBYTECODE" not in original


def test_verifier_rejects_imports_outside_the_child_venv(tmp_path: Path) -> None:
    verifier = _verifier()
    child_venv = tmp_path / "child"
    installed = child_venv / "lib" / "python" / "site-packages" / "troupe"
    installed.mkdir(parents=True)
    wrapper = installed / "__init__.py"
    runtime = installed / "_runtime.abi3.so"
    dependency = installed.parent / "troupe_smoke_dependency.py"
    wrapper.touch()
    runtime.touch()
    dependency.touch()

    payload = {
        "troupe_file": str(wrapper),
        "runtime_file": str(runtime),
        "dependency_file": str(dependency),
    }

    verifier._validate_installed_paths(child_venv, payload)

    outside = tmp_path / "source" / "outside.bin"
    outside.parent.mkdir(parents=True)
    outside.touch()
    for key in ("troupe_file", "runtime_file", "dependency_file"):
        with pytest.raises(verifier.VerificationError):
            verifier._validate_installed_paths(
                child_venv,
                {**payload, key: str(outside)},
            )

        linked = installed.parent / f"linked-{key}"
        linked.symlink_to(outside)
        with pytest.raises(verifier.VerificationError):
            verifier._validate_installed_paths(
                child_venv,
                {**payload, key: str(linked)},
            )


def test_verifier_parser_accepts_only_the_two_exact_modes(tmp_path: Path) -> None:
    verifier = _verifier()
    parser = verifier._parser()
    output = tmp_path / "published"
    wheel = tmp_path / "troupe.whl"
    checksum = tmp_path / "SHA256SUMS"

    build = parser.parse_args(
        [
            "--build",
            "--release",
            "--target",
            "x86_64-unknown-linux-gnu",
            "--manylinux",
            "2_17",
            "--output-dir",
            str(output),
        ]
    )
    assert vars(build) == {
        "build": True,
        "wheel": None,
        "sha256_file": None,
        "release": True,
        "target": "x86_64-unknown-linux-gnu",
        "manylinux": "2_17",
        "output_dir": output,
    }

    supplied = parser.parse_args(
        ["--wheel", str(wheel), "--sha256-file", str(checksum)]
    )
    assert vars(supplied) == {
        "build": False,
        "wheel": wheel,
        "sha256_file": checksum,
        "release": False,
        "target": None,
        "manylinux": None,
        "output_dir": None,
    }


@pytest.mark.parametrize(
    "arguments",
    [
        [],
        ["--build", "--wheel", "wheel.whl", "--sha256-file", "SHA256SUMS"],
        ["--wheel", "wheel.whl"],
        ["--sha256-file", "SHA256SUMS"],
        ["--build", "--sha256-file", "SHA256SUMS"],
        ["--wheel", "wheel.whl", "--sha256-file", "SHA256SUMS", "--release"],
        [
            "--wheel",
            "wheel.whl",
            "--sha256-file",
            "SHA256SUMS",
            "--target",
            "x86_64-unknown-linux-gnu",
        ],
        [
            "--wheel",
            "wheel.whl",
            "--sha256-file",
            "SHA256SUMS",
            "--manylinux",
            "2_17",
        ],
        [
            "--wheel",
            "wheel.whl",
            "--sha256-file",
            "SHA256SUMS",
            "--output-dir",
            "out",
        ],
    ],
)
def test_verifier_parser_rejects_missing_mixed_or_mode_specific_arguments(
    arguments: list[str],
) -> None:
    verifier = _verifier()

    with pytest.raises(SystemExit) as raised:
        verifier._parser().parse_args(arguments)
    assert raised.value.code == 2


def test_sha256_file_is_strict_and_bound_to_the_exact_wheel(tmp_path: Path) -> None:
    verifier = _verifier()
    wheel = tmp_path / "troupe-0.1.0-cp310-abi3-manylinux_2_17_x86_64.whl"
    wheel.write_bytes(b"wheel bytes")
    checksum = tmp_path / "SHA256SUMS"
    digest = hashlib.sha256(wheel.read_bytes()).hexdigest()

    verifier._write_sha256(wheel, checksum)
    assert checksum.read_bytes() == f"{digest}  {wheel.name}\n".encode("ascii")
    verifier._validate_sha256(wheel, checksum)

    invalid = (
        f"{'0' * 64}  {wheel.name}\n",
        f"{digest}  other.whl\n",
        f"{digest.upper()}  {wheel.name}\n",
        f"{digest} {wheel.name}\n",
        f"{digest}\t{wheel.name}\n",
        f"{digest}  /tmp/{wheel.name}\n",
        f"{digest}  ./{wheel.name}\n",
        f"{digest}  {wheel.name}",
        f"{digest}  {wheel.name}\n\n",
        f"{digest}  {wheel.name}\n{digest}  {wheel.name}\n",
    )
    for index, content in enumerate(invalid):
        bad = tmp_path / f"bad-{index}"
        bad.write_text(content, encoding="ascii")
        with pytest.raises(verifier.VerificationError):
            verifier._validate_sha256(wheel, bad)

    non_ascii = tmp_path / "non-ascii"
    non_ascii.write_bytes(f"{digest}  ".encode("ascii") + b"\xff.whl\n")
    with pytest.raises(verifier.VerificationError):
        verifier._validate_sha256(wheel, non_ascii)


def _fake_build_run(
    events: list[str],
    *,
    expected_release: bool = False,
    expected_target: str | None = None,
    expected_manylinux: str | None = None,
):
    def run(
        command: list[str],
        *,
        cwd: Path,
        env: dict[str, str],
        **_: object,
    ) -> str:
        events.append("run-build")
        output = Path(command[command.index("--out") + 1])
        assert cwd == ROOT
        assert "CONDA_PREFIX" not in env
        verifier_command = [
            "maturin",
            "build",
            "--sdist",
            "--locked",
            "--manifest-path",
            "rust/Cargo.toml",
            "--out",
            str(output),
        ]
        if expected_release:
            verifier_command.append("--release")
        if expected_target is not None:
            verifier_command.extend(["--target", expected_target])
        if expected_manylinux is not None:
            verifier_command.extend(["--manylinux", expected_manylinux])
        assert command == verifier_command
        output.mkdir(parents=True, exist_ok=True)
        (output / "troupe-0.1.0.tar.gz").touch()
        (output / "troupe-0.1.0-cp310-abi3-manylinux_2_17_x86_64.whl").touch()
        return ""

    return run


def _patch_recording_validators(
    monkeypatch: pytest.MonkeyPatch,
    verifier: ModuleType,
    events: list[str],
    *,
    expected_manylinux: str | None = None,
) -> None:
    def source(*_: object, **__: object) -> tuple[bytes, bytes]:
        events.append("source")
        return EXPECTED_WRAPPER, EXPECTED_STUB

    def record(name: str):
        def inner(*_: object, **__: object) -> None:
            events.append(name)

        return inner

    monkeypatch.setattr(verifier, "_validate_source", source)
    monkeypatch.setattr(verifier, "_validate_sdist", record("sdist"))
    def wheel(
        *_: object,
        required_manylinux: str | None,
        **__: object,
    ) -> None:
        assert required_manylinux == expected_manylinux
        events.append("wheel")

    monkeypatch.setattr(verifier, "_validate_wheel", wheel)
    monkeypatch.setattr(verifier, "_smoke_wheel", record("smoke"))


def test_build_mode_records_one_build_then_all_evidence_before_publish(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    verifier = _verifier()
    output = tmp_path / "published"
    events: list[str] = []
    commits = 0

    monkeypatch.setenv("CONDA_PREFIX", "/outer-conda")
    monkeypatch.setattr(
        verifier,
        "_run",
        _fake_build_run(
            events,
            expected_release=True,
            expected_target="x86_64-unknown-linux-gnu",
            expected_manylinux="2_17",
        ),
    )
    _patch_recording_validators(
        monkeypatch,
        verifier,
        events,
        expected_manylinux="2_17",
    )

    def stage(wheel: Path, destination: Path) -> Path:
        assert wheel.suffix == ".whl"
        assert destination == output
        events.append("publish")
        staging = tmp_path / ".published-staging"
        staging.mkdir()
        (staging / wheel.name).touch()
        (staging / "SHA256SUMS").write_text("placeholder", encoding="ascii")
        return staging

    def commit(staging: Path, destination: Path) -> None:
        nonlocal commits
        commits += 1
        os.rename(staging, destination)

    monkeypatch.setattr(verifier, "_stage_publication", stage)
    monkeypatch.setattr(verifier, "_commit_publication", commit)
    monkeypatch.setattr(
        verifier,
        "_validate_sha256",
        lambda *_args, **_kwargs: pytest.fail("build mode must not consume a SHA file"),
    )

    assert verifier.main(
        [
            "--build",
            "--release",
            "--target",
            "x86_64-unknown-linux-gnu",
            "--manylinux",
            "2_17",
            "--output-dir",
            str(output),
        ]
    ) == 0
    assert events == ["run-build", "source", "sdist", "wheel", "smoke", "publish"]
    assert commits == 1
    assert output.is_dir()


def test_build_mode_never_publishes_when_smoke_fails(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    verifier = _verifier()
    output = tmp_path / "published"
    events: list[str] = []
    monkeypatch.setattr(verifier, "_run", _fake_build_run(events))
    _patch_recording_validators(monkeypatch, verifier, events)

    def fail_smoke(*_: object, **__: object) -> None:
        events.append("smoke")
        raise verifier.VerificationError("smoke failed")

    monkeypatch.setattr(verifier, "_smoke_wheel", fail_smoke)
    monkeypatch.setattr(
        verifier,
        "_stage_publication",
        lambda *_args, **_kwargs: pytest.fail("failed smoke must not publish"),
    )
    monkeypatch.setattr(
        verifier,
        "_commit_publication",
        lambda *_args, **_kwargs: pytest.fail("failed smoke must not commit"),
    )

    assert verifier.main(["--build", "--output-dir", str(output)]) == 1
    assert events == ["run-build", "source", "sdist", "wheel", "smoke"]
    assert not output.exists()


def test_supplied_wheel_mode_hashes_first_and_never_builds_or_reads_sdist(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    verifier = _verifier()
    wheel = tmp_path / "troupe.whl"
    checksum = tmp_path / "SHA256SUMS"
    wheel.touch()
    checksum.touch()
    events: list[str] = []

    monkeypatch.setattr(verifier, "_validate_sha256", lambda *_: events.append("hash"))
    _patch_recording_validators(monkeypatch, verifier, events)
    monkeypatch.setattr(
        verifier,
        "_validate_sdist",
        lambda *_args, **_kwargs: pytest.fail("wheel mode must not inspect an sdist"),
    )
    monkeypatch.setattr(
        verifier,
        "_run",
        lambda *_args, **_kwargs: pytest.fail("wheel mode must not run maturin"),
    )
    monkeypatch.setattr(
        verifier,
        "_stage_publication",
        lambda *_args, **_kwargs: pytest.fail("wheel mode must not publish"),
    )

    assert verifier.main(
        ["--wheel", str(wheel), "--sha256-file", str(checksum)]
    ) == 0
    assert events == ["hash", "source", "wheel", "smoke"]


def test_wrong_hash_short_circuits_every_other_boundary(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    verifier = _verifier()
    wheel = tmp_path / "troupe.whl"
    checksum = tmp_path / "SHA256SUMS"
    wheel.write_bytes(b"wheel")
    checksum.write_text(f"{'0' * 64}  {wheel.name}\n", encoding="ascii")
    calls = 0

    def forbidden(*_: object, **__: object) -> None:
        nonlocal calls
        calls += 1

    for name in (
        "_validate_source",
        "_validate_sdist",
        "_validate_wheel",
        "_smoke_wheel",
        "_run",
        "_stage_publication",
        "_commit_publication",
    ):
        monkeypatch.setattr(verifier, name, forbidden)

    assert verifier.main(
        ["--wheel", str(wheel), "--sha256-file", str(checksum)]
    ) == 1
    assert calls == 0


def test_output_directory_must_not_exist_before_build(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    verifier = _verifier()
    output = tmp_path / "published"
    output.mkdir()
    runs = 0

    def run(*_: object, **__: object) -> str:
        nonlocal runs
        runs += 1
        return ""

    monkeypatch.setattr(verifier, "_run", run)
    assert verifier.main(["--build", "--output-dir", str(output)]) == 1
    assert runs == 0


@pytest.mark.parametrize("pattern", ["*.whl", "*.tar.gz"])
@pytest.mark.parametrize("count", [0, 2])
def test_build_artifact_cardinality_must_be_exactly_one(
    tmp_path: Path,
    pattern: str,
    count: int,
) -> None:
    verifier = _verifier()
    suffix = ".whl" if pattern == "*.whl" else ".tar.gz"
    for index in range(count):
        (tmp_path / f"artifact-{index}{suffix}").touch()

    with pytest.raises(verifier.VerificationError):
        verifier._only_artifact(tmp_path, pattern)


@pytest.mark.parametrize(
    ("artifact", "count"),
    [("wheel", 0), ("wheel", 2), ("sdist", 0), ("sdist", 2)],
)
def test_build_main_rejects_wrong_artifact_cardinality_before_validation(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
    artifact: str,
    count: int,
) -> None:
    verifier = _verifier()
    output = tmp_path / "published"

    def run(
        command: list[str],
        *,
        cwd: Path,
        env: dict[str, str],
        **_: object,
    ) -> str:
        assert cwd == ROOT
        assert "CONDA_PREFIX" not in env
        build_output = Path(command[command.index("--out") + 1])
        build_output.mkdir(parents=True)
        wheel_count = count if artifact == "wheel" else 1
        sdist_count = count if artifact == "sdist" else 1
        for index in range(wheel_count):
            (build_output / f"troupe-{index}-cp310-abi3-manylinux_2_17_x86_64.whl").touch()
        for index in range(sdist_count):
            (build_output / f"troupe-{index}.tar.gz").touch()
        return ""

    monkeypatch.setattr(verifier, "_run", run)
    for name in (
        "_validate_source",
        "_validate_sdist",
        "_validate_wheel",
        "_smoke_wheel",
        "_stage_publication",
    ):
        monkeypatch.setattr(
            verifier,
            name,
            lambda *_args, _name=name, **_kwargs: pytest.fail(
                f"{_name} must not run after invalid artifact cardinality"
            ),
        )

    assert verifier.main(["--build", "--output-dir", str(output)]) == 1
    assert not output.exists()


def test_publication_is_staged_then_atomically_renamed(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    verifier = _verifier()
    wheel = tmp_path / "troupe.whl"
    wheel.write_bytes(b"wheel")
    output = tmp_path / "wheel-artifact"
    parent = output.parent.stat()

    def forbidden_stage_metadata(*_: object) -> None:
        pytest.fail("publication metadata must not change while staging")

    with monkeypatch.context() as stage_metadata:
        stage_metadata.setattr(verifier.Path, "chmod", forbidden_stage_metadata)
        stage_metadata.setattr(verifier.os, "chown", forbidden_stage_metadata)
        staging = verifier._stage_publication(wheel, output)
    staged_wheel = staging / wheel.name
    checksum = staging / "SHA256SUMS"
    assert staging.parent == output.parent
    assert staging != output
    assert not output.exists()
    assert sorted(path.name for path in staging.iterdir()) == ["SHA256SUMS", wheel.name]
    assert staging.stat().st_mode & 0o777 == 0o700
    assert staging.stat().st_uid == os.geteuid()

    events: list[str] = []
    real_validate_sha256 = verifier._validate_sha256

    def validate_sha256(staged: Path, recorded_checksum: Path) -> None:
        events.append("recheck")
        real_validate_sha256(staged, recorded_checksum)

    chmod_calls: list[tuple[Path, int]] = []
    real_chmod = verifier.Path.chmod

    def chmod(path: Path, mode: int) -> None:
        chmod_calls.append((path, mode))
        events.append(f"chmod:{path.name}")
        real_chmod(path, mode)

    chown_calls: list[tuple[Path, int, int]] = []
    real_chown = verifier.os.chown

    def chown(path: Path, uid: int, gid: int) -> None:
        resolved = Path(path)
        chown_calls.append((resolved, uid, gid))
        events.append(f"chown:{resolved.name}")
        real_chown(path, uid, gid)

    rename_calls: list[tuple[Path, Path]] = []
    real_rename = verifier.os.rename

    def rename(source: Path, destination: Path) -> None:
        assert events == [
            "recheck",
            f"chmod:{wheel.name}",
            "chmod:SHA256SUMS",
            f"chmod:{staging.name}",
            f"chown:{wheel.name}",
            "chown:SHA256SUMS",
            f"chown:{staging.name}",
        ]
        assert stat.S_IMODE(staging.stat().st_mode) == 0o755
        assert stat.S_IMODE(staged_wheel.stat().st_mode) == 0o644
        assert stat.S_IMODE(checksum.stat().st_mode) == 0o644
        assert {
            (path.stat().st_uid, path.stat().st_gid)
            for path in (staging, staged_wheel, checksum)
        } == {(parent.st_uid, parent.st_gid)}
        events.append("rename")
        rename_calls.append((source, destination))
        real_rename(source, destination)

    monkeypatch.setattr(verifier, "_validate_sha256", validate_sha256)
    monkeypatch.setattr(verifier.Path, "chmod", chmod)
    monkeypatch.setattr(verifier.os, "chown", chown)
    monkeypatch.setattr(verifier.os, "rename", rename)

    verifier._commit_publication(staging, output)
    assert set(chmod_calls) == {
        (staging, 0o755),
        (staged_wheel, 0o644),
        (checksum, 0o644),
    }
    assert set(chown_calls) == {
        (staging, parent.st_uid, parent.st_gid),
        (staged_wheel, parent.st_uid, parent.st_gid),
        (checksum, parent.st_uid, parent.st_gid),
    }
    assert rename_calls == [(staging, output)]
    assert events[-1] == "rename"
    assert output.is_dir()
    assert not staging.exists()
    assert sorted(path.name for path in output.iterdir()) == ["SHA256SUMS", wheel.name]
    real_validate_sha256(output / wheel.name, output / "SHA256SUMS")
    assert stat.S_IMODE(output.stat().st_mode) == 0o755
    assert stat.S_IMODE((output / wheel.name).stat().st_mode) == 0o644
    assert stat.S_IMODE((output / "SHA256SUMS").stat().st_mode) == 0o644
    assert (output.stat().st_uid, output.stat().st_gid) == (
        parent.st_uid,
        parent.st_gid,
    )


def test_publication_owner_comes_from_output_parent(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    verifier = _verifier()
    wheel = tmp_path / "troupe.whl"
    wheel.write_bytes(b"wheel")
    output = tmp_path / "wheel-artifact"
    staging = verifier._stage_publication(wheel, output)
    sentinel_uid = os.geteuid() + 10_000
    sentinel_gid = os.getegid() + 20_000
    real_stat = verifier.Path.stat

    class ParentStat:
        st_uid = sentinel_uid
        st_gid = sentinel_gid

    def path_stat(path: Path, *args: object, **kwargs: object) -> object:
        if path == output.parent:
            return ParentStat()
        return real_stat(path, *args, **kwargs)

    chown_calls: list[tuple[Path, int, int]] = []

    def chown(path: Path, uid: int, gid: int) -> None:
        chown_calls.append((Path(path), uid, gid))

    monkeypatch.setattr(verifier.Path, "stat", path_stat)
    monkeypatch.setattr(verifier.os, "chown", chown)

    verifier._commit_publication(staging, output)

    assert chown_calls == [
        (staging / wheel.name, sentinel_uid, sentinel_gid),
        (staging / "SHA256SUMS", sentinel_uid, sentinel_gid),
        (staging, sentinel_uid, sentinel_gid),
    ]
    assert output.is_dir()


def test_publication_failure_cleans_staging_and_leaves_output_absent(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    verifier = _verifier()
    wheel = tmp_path / "troupe.whl"
    wheel.write_bytes(b"wheel")
    output = tmp_path / "wheel-artifact"

    monkeypatch.setattr(shutil, "copy2", lambda *_: (_ for _ in ()).throw(OSError("copy")))
    with pytest.raises(verifier.VerificationError):
        verifier._stage_publication(wheel, output)
    assert not output.exists()
    assert not list(tmp_path.glob(".wheel-artifact-*"))


def test_publication_abort_cleans_staging_and_preserves_base_exception(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    verifier = _verifier()
    wheel = tmp_path / "troupe.whl"
    wheel.write_bytes(b"wheel")
    output = tmp_path / "wheel-artifact"

    class PublicationAbort(BaseException):
        pass

    abort = PublicationAbort("stage interrupted")

    def abort_copy(*_: object, **__: object) -> None:
        raise abort

    monkeypatch.setattr(verifier.shutil, "copy2", abort_copy)
    with pytest.raises(PublicationAbort) as captured:
        verifier._stage_publication(wheel, output)
    assert captured.value is abort
    assert not output.exists()
    assert not list(tmp_path.glob(".wheel-artifact-*"))


@pytest.mark.parametrize("operation", ["mode", "owner"])
@pytest.mark.parametrize("target_name", ["directory", "wheel", "checksum"])
def test_publication_metadata_failure_cleans_staging(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
    operation: str,
    target_name: str,
) -> None:
    verifier = _verifier()
    wheel = tmp_path / "troupe.whl"
    wheel.write_bytes(b"wheel")
    output = tmp_path / "wheel-artifact"
    staging = verifier._stage_publication(wheel, output)
    targets = {
        "directory": staging,
        "wheel": staging / wheel.name,
        "checksum": staging / "SHA256SUMS",
    }
    target = targets[target_name]

    if operation == "mode":
        real_chmod = verifier.Path.chmod

        def chmod(path: Path, mode: int) -> None:
            if path == target:
                raise OSError(f"{operation} failure on {target_name}")
            real_chmod(path, mode)

        monkeypatch.setattr(verifier.Path, "chmod", chmod)
    else:
        real_chown = verifier.os.chown

        def chown(path: Path, uid: int, gid: int) -> None:
            if Path(path) == target:
                raise OSError(f"{operation} failure on {target_name}")
            real_chown(path, uid, gid)

        monkeypatch.setattr(verifier.os, "chown", chown)

    monkeypatch.setattr(
        verifier.os,
        "rename",
        lambda *_: pytest.fail("rename must not run after publication metadata failure"),
    )

    with pytest.raises(verifier.VerificationError):
        verifier._commit_publication(staging, output)
    assert not staging.exists()
    assert not output.exists()
    assert not list(tmp_path.glob(".wheel-artifact-*"))


def test_commit_checksum_failure_precedes_metadata_and_rename(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    verifier = _verifier()
    wheel = tmp_path / "troupe.whl"
    wheel.write_bytes(b"wheel")
    output = tmp_path / "wheel-artifact"
    staging = verifier._stage_publication(wheel, output)

    def fail_recheck(*_: object) -> None:
        raise verifier.VerificationError("checksum recheck")

    def forbidden(*_: object) -> None:
        pytest.fail("metadata and rename must not run after checksum recheck failure")

    monkeypatch.setattr(verifier, "_validate_sha256", fail_recheck)
    monkeypatch.setattr(verifier.Path, "chmod", forbidden)
    monkeypatch.setattr(verifier.os, "chown", forbidden)
    monkeypatch.setattr(verifier.os, "rename", forbidden)

    with pytest.raises(verifier.VerificationError):
        verifier._commit_publication(staging, output)
    assert not staging.exists()
    assert not output.exists()
    assert not list(tmp_path.glob(".wheel-artifact-*"))


@pytest.mark.parametrize("boundary", ["write", "recheck"])
def test_checksum_failure_cleans_staging_and_leaves_output_absent(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
    boundary: str,
) -> None:
    verifier = _verifier()
    wheel = tmp_path / "troupe.whl"
    wheel.write_bytes(b"wheel")
    output = tmp_path / "wheel-artifact"

    if boundary == "write":
        monkeypatch.setattr(
            verifier,
            "_write_sha256",
            lambda *_: (_ for _ in ()).throw(OSError("checksum write")),
        )
    else:
        monkeypatch.setattr(
            verifier,
            "_validate_sha256",
            lambda *_: (_ for _ in ()).throw(
                verifier.VerificationError("checksum recheck")
            ),
        )

    with pytest.raises(verifier.VerificationError):
        verifier._stage_publication(wheel, output)
    assert not output.exists()
    assert not list(tmp_path.glob(".wheel-artifact-*"))


def test_rename_failure_cleans_staging_and_leaves_output_absent(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    verifier = _verifier()
    wheel = tmp_path / "troupe.whl"
    wheel.write_bytes(b"wheel")
    output = tmp_path / "wheel-artifact"
    staging = verifier._stage_publication(wheel, output)
    monkeypatch.setattr(
        verifier.os,
        "rename",
        lambda *_: (_ for _ in ()).throw(OSError("rename")),
    )

    with pytest.raises(verifier.VerificationError):
        verifier._commit_publication(staging, output)
    assert not staging.exists()
    assert not output.exists()


def test_commit_abort_cleans_staging_and_preserves_base_exception(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    verifier = _verifier()
    wheel = tmp_path / "troupe.whl"
    wheel.write_bytes(b"wheel")
    output = tmp_path / "wheel-artifact"
    staging = verifier._stage_publication(wheel, output)

    class PublicationAbort(BaseException):
        pass

    abort = PublicationAbort("commit interrupted")

    def abort_chmod(*_: object, **__: object) -> None:
        raise abort

    monkeypatch.setattr(verifier.Path, "chmod", abort_chmod)
    with pytest.raises(PublicationAbort) as captured:
        verifier._commit_publication(staging, output)
    assert captured.value is abort
    assert not staging.exists()
    assert not output.exists()
    assert not list(tmp_path.glob(".wheel-artifact-*"))


def test_build_cleanup_finishes_before_atomic_commit(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    verifier = _verifier()
    output = tmp_path / "published"
    build_workspace = tmp_path / "build-workspace"
    events: list[str] = []

    class RecordingTemporaryDirectory:
        def __init__(self, **_: object) -> None:
            pass

        def __enter__(self) -> str:
            build_workspace.mkdir()
            return str(build_workspace)

        def __exit__(self, *_: object) -> None:
            shutil.rmtree(build_workspace)
            events.append("cleanup")

    monkeypatch.setattr(verifier.tempfile, "TemporaryDirectory", RecordingTemporaryDirectory)
    monkeypatch.setattr(verifier, "_run", _fake_build_run(events))
    _patch_recording_validators(monkeypatch, verifier, events)

    def stage(wheel: Path, destination: Path) -> Path:
        events.append("publish")
        staging = tmp_path / ".published-staging"
        staging.mkdir()
        (staging / wheel.name).touch()
        return staging

    def commit(staging: Path, destination: Path) -> None:
        events.append("commit")
        os.rename(staging, destination)

    monkeypatch.setattr(verifier, "_stage_publication", stage)
    monkeypatch.setattr(verifier, "_commit_publication", commit)

    assert verifier.main(["--build", "--output-dir", str(output)]) == 0
    assert events.index("publish") < events.index("cleanup") < events.index("commit")


def test_build_cleanup_failure_never_exposes_final_output(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    verifier = _verifier()
    output = tmp_path / "published"
    build_workspace = tmp_path / "build-workspace"
    staged: list[Path] = []
    events: list[str] = []

    class FailingTemporaryDirectory:
        def __init__(self, **_: object) -> None:
            pass

        def __enter__(self) -> str:
            build_workspace.mkdir()
            return str(build_workspace)

        def __exit__(self, *_: object) -> None:
            shutil.rmtree(build_workspace)
            raise OSError("temporary cleanup failed")

    monkeypatch.setattr(verifier.tempfile, "TemporaryDirectory", FailingTemporaryDirectory)
    monkeypatch.setattr(verifier, "_run", _fake_build_run(events))
    _patch_recording_validators(monkeypatch, verifier, events)
    real_stage = verifier._stage_publication

    def recording_stage(wheel: Path, destination: Path) -> Path:
        result = real_stage(wheel, destination)
        staged.append(result)
        return result

    monkeypatch.setattr(verifier, "_stage_publication", recording_stage)

    assert verifier.main(["--build", "--output-dir", str(output)]) == 1
    assert not output.exists()
    assert staged and all(not path.exists() for path in staged)


def test_build_cleanup_abort_discards_staging_and_preserves_base_exception(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    verifier = _verifier()
    output = tmp_path / "published"
    build_workspace = tmp_path / "build-workspace"
    staged: list[Path] = []
    events: list[str] = []

    class BuildAbort(BaseException):
        pass

    abort = BuildAbort("temporary cleanup interrupted")

    class AbortingTemporaryDirectory:
        def __init__(self, **_: object) -> None:
            pass

        def __enter__(self) -> str:
            build_workspace.mkdir()
            return str(build_workspace)

        def __exit__(self, *_: object) -> None:
            shutil.rmtree(build_workspace)
            raise abort

    monkeypatch.setattr(verifier.tempfile, "TemporaryDirectory", AbortingTemporaryDirectory)
    monkeypatch.setattr(verifier, "_run", _fake_build_run(events))
    _patch_recording_validators(monkeypatch, verifier, events)
    real_stage = verifier._stage_publication

    def recording_stage(wheel: Path, destination: Path) -> Path:
        staging = real_stage(wheel, destination)
        staged.append(staging)
        return staging

    monkeypatch.setattr(verifier, "_stage_publication", recording_stage)
    with pytest.raises(BuildAbort) as captured:
        verifier.main(["--build", "--output-dir", str(output)])
    assert captured.value is abort
    assert not output.exists()
    assert staged and all(not path.exists() for path in staged)
    assert not list(tmp_path.glob(".published-*"))


def test_dependency_wheel_is_generated_with_valid_metadata_and_record(
    tmp_path: Path,
) -> None:
    verifier = _verifier()

    wheel = verifier._build_dependency_wheel(tmp_path)
    assert wheel.name == "troupe_smoke_dependency-1.0.0-py3-none-any.whl"
    with WheelFile(wheel) as archive:
        names = sorted(name for name in archive.namelist() if not name.endswith("/"))
        for name in names:
            archive.read(name)
        assert names == [
            "troupe_smoke_dependency-1.0.0.dist-info/METADATA",
            "troupe_smoke_dependency-1.0.0.dist-info/RECORD",
            "troupe_smoke_dependency-1.0.0.dist-info/WHEEL",
            "troupe_smoke_dependency.py",
        ]
        assert archive.read("troupe_smoke_dependency.py") == (
            ROOT / "tests" / "fixtures" / "wheel_smoke_dependency.py"
        ).read_bytes()
        metadata = archive.read(
            "troupe_smoke_dependency-1.0.0.dist-info/METADATA"
        ).decode("utf-8")
        assert "Name: troupe-smoke-dependency\n" in metadata
        assert "Version: 1.0.0\n" in metadata
        assert "Requires-Dist:" not in metadata
        wheel_metadata = archive.read(
            "troupe_smoke_dependency-1.0.0.dist-info/WHEEL"
        ).decode("utf-8")
        assert "Wheel-Version: 1.0\n" in wheel_metadata
        assert "Root-Is-Purelib: true\n" in wheel_metadata
        assert "Tag: py3-none-any\n" in wheel_metadata

    with zipfile.ZipFile(wheel) as archive:
        infos = [info for info in archive.infolist() if not info.is_dir()]
        assert len({info.filename for info in infos}) == len(infos)
        record_name = "troupe_smoke_dependency-1.0.0.dist-info/RECORD"
        rows = list(
            csv.reader(io.StringIO(archive.read(record_name).decode("utf-8")))
        )
        assert all(len(row) == 3 for row in rows)
        assert len({row[0] for row in rows}) == len(rows)
        assert {row[0] for row in rows} == {info.filename for info in infos}
        for path, encoded_hash, size in rows:
            if path == record_name:
                assert (encoded_hash, size) == ("", "")
                continue
            data = archive.read(path)
            digest = base64.urlsafe_b64encode(hashlib.sha256(data).digest()).rstrip(b"=")
            assert encoded_hash == f"sha256={digest.decode('ascii')}"
            assert size == str(len(data))


def _installed_smoke_payload(child: Path) -> dict[str, object]:
    installed = child / "lib" / "python" / "site-packages"
    return {
        "troupe_file": str(installed / "troupe" / "__init__.py"),
        "runtime_file": str(installed / "troupe" / "_runtime.abi3.so"),
        "dependency_file": str(installed / "troupe_smoke_dependency.py"),
        "production_identity": True,
        "production_module": "troupe",
        "exports": PUBLIC_EXPORTS,
        "public_identities": True,
        "public_modules": True,
        "schema_contract": True,
        "agent_test_support_absent": True,
        "native_construction_gates": True,
        "gil_disabled": False,
        "surrogate_constructor": True,
        "default_hooks": True,
        "subclass_override": True,
        "entry_points": [["troupe", "troupe._runtime:main"]],
    }


def _installed_smoke_events(raw_args: list[str]) -> list[list[object]]:
    scene = "scene-123e4567-e89b-42d3-a456-426614174000"
    downstream_id = f"{scene}-cue1"
    effect_id = f"{downstream_id}-effect0"
    return [
        ["args", raw_args],
        ["start"],
        ["scene", "dependency-ok", "module-ok", "resource-ok"],
        [
            "actor-round-trip",
            {
                "constructors": [["router", "router", True], ["worker", "worker", True]],
                "queries": {"exact": "router", "pattern": ["router", "worker"]},
                "root_cue": {"id": f"{scene}-cue0", "source": scene},
                "downstream_cue": {"id": downstream_id, "source": "router"},
                "effect": {"id": effect_id, "owner": "worker", "value": "mutated"},
                "result": {
                    "type": "tuple",
                    "items": [[effect_id, "worker", "mutated"]],
                },
                "threads": [4242] * 8,
            },
        ],
        [
            "cancellation",
            {
                "admitted_snapshot": "before-release",
                "pre_release": {
                    "caller_done": False,
                    "successor_done": False,
                    "successor_entered": False,
                },
                "other_actor_result": [],
                "completion_saw_release": {"caller": True, "successor": True},
                "caller_outcome": "CancelledError",
                "successor_result": [],
            },
        ],
        ["stop"],
    ]


def _mock_agent_events(pids: tuple[int, int] = (101, 102)) -> str:
    rows: list[dict[str, object]] = []
    for pid in pids:
        rows.extend(
            [
                {"event": "process_started", "pid": pid},
                {"event": "session_new_received", "pid": pid},
                {"event": "mcp_tools_list", "pid": pid},
                {"event": "config_applied", "pid": pid, "config_id": "mode"},
                {"event": "config_applied", "pid": pid, "config_id": "model"},
            ]
        )
    return "".join(f"{json.dumps(row)}\n" for row in rows)


def _load_wheel_smoke_production(monkeypatch: pytest.MonkeyPatch) -> ModuleType:
    package_root = ROOT / "tests" / "fixtures" / "productions"
    dependency = ModuleType("troupe_smoke_dependency")
    dependency.VALUE = "dependency-ok"
    dependency.__file__ = "/installed/troupe_smoke_dependency.py"
    monkeypatch.setitem(sys.modules, "troupe_smoke_dependency", dependency)
    monkeypatch.syspath_prepend(str(package_root))
    monkeypatch.setattr(sys, "dont_write_bytecode", True)
    for name in tuple(sys.modules):
        if name == "wheel_smoke_production" or name.startswith("wheel_smoke_production."):
            monkeypatch.delitem(sys.modules, name)
    return importlib.import_module("wheel_smoke_production.production")


def _producer_observations() -> tuple[dict[str, object], dict[str, object]]:
    scene = "scene-123e4567-e89b-42d3-a456-426614174000"
    downstream_id = f"{scene}-cue1"
    effect_id = f"{downstream_id}-effect0"
    actor = {
        "router": SimpleNamespace(name="router"),
        "worker": SimpleNamespace(name="worker"),
        "constructors": [["router", "router", True], ["worker", "worker", True]],
        "exact": SimpleNamespace(name="router"),
        "pattern": [SimpleNamespace(name="router"), SimpleNamespace(name="worker")],
        "root_cue": SimpleNamespace(id=f"{scene}-cue0", source=scene),
        "downstream_cue": SimpleNamespace(id=downstream_id, source="router"),
        "result": (SimpleNamespace(id=effect_id, owner="worker", value="mutated"),),
        "threads": [4242] * 8,
    }
    cancellation = {
        "admitted_snapshot": "before-release",
        "caller_done": False,
        "successor_done": False,
        "successor_entered": False,
        "other_actor_result": (),
        "caller_completion_saw_release": True,
        "successor_completion_saw_release": True,
        "caller_outcome": "CancelledError",
        "successor_result": (),
    }
    return actor, cancellation


@pytest.mark.parametrize(
    ("key", "value"),
    [
        ("production_identity", False),
        ("production_module", "troupe._runtime"),
        ("exports", []),
        ("exports", [*PUBLIC_EXPORTS, "Other"]),
        ("exports", ["Other"]),
        ("public_identities", False),
        ("public_modules", False),
        ("agent_test_support_absent", False),
        ("native_construction_gates", False),
        ("gil_disabled", True),
        ("surrogate_constructor", False),
        ("default_hooks", False),
        ("subclass_override", False),
        ("entry_points", []),
        ("entry_points", [["troupe", "troupe:main"]]),
        (
            "entry_points",
            [
                ["troupe", "troupe._runtime:main"],
                ["extra", "troupe._runtime:main"],
            ],
        ),
    ],
)
def test_smoke_payload_rejects_every_wrong_api_or_interpreter_fact(
    tmp_path: Path,
    key: str,
    value: object,
) -> None:
    verifier = _verifier()
    child = tmp_path / "child"
    payload = _installed_smoke_payload(child)
    for path_key in ("troupe_file", "runtime_file", "dependency_file"):
        Path(str(payload[path_key])).parent.mkdir(parents=True, exist_ok=True)
        Path(str(payload[path_key])).touch()
    payload[key] = value

    with pytest.raises(verifier.VerificationError):
        verifier._validate_smoke_payload(child, payload)


@pytest.mark.parametrize("missing", list(_installed_smoke_payload(Path("/child"))))
def test_smoke_payload_requires_every_field(tmp_path: Path, missing: str) -> None:
    verifier = _verifier()
    child = tmp_path / "child"
    payload = _installed_smoke_payload(child)
    for path_key in ("troupe_file", "runtime_file", "dependency_file"):
        installed_file = Path(str(payload[path_key]))
        installed_file.parent.mkdir(parents=True, exist_ok=True)
        installed_file.touch()
    payload.pop(missing)

    with pytest.raises(verifier.VerificationError):
        verifier._validate_smoke_payload(child, payload)


def test_smoke_payload_consumes_the_installed_path_validator(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    verifier = _verifier()
    child = tmp_path / "child"
    payload = _installed_smoke_payload(child)
    calls: list[tuple[Path, dict[str, object]]] = []

    def paths(root: Path, received: dict[str, object]) -> None:
        calls.append((root, received))

    monkeypatch.setattr(verifier, "_validate_installed_paths", paths)
    verifier._validate_smoke_payload(child, payload)
    assert calls == [(child, payload)]


def _execute_child_probe(
    verifier: ModuleType,
    monkeypatch: pytest.MonkeyPatch,
    mutation: str | None,
) -> dict[str, object]:
    class ProbeAwaitable:
        def __init__(self, result: object = None, error: Exception | None = None) -> None:
            self.result = result
            self.error = error

        def __await__(self) -> Generator[None, None, object]:
            if False:
                yield None
            if self.error is not None:
                raise self.error
            return self.result

    class ProbeProduction:
        def __init_subclass__(cls, **kwargs: object) -> None:
            if mutation == "subclass":
                raise RuntimeError("subclass disabled")
            super().__init_subclass__(**kwargs)

        def __init__(self, *positional: object, **keywords: object) -> None:
            if mutation == "constructor":
                return
            if keywords or len(positional) != 1:
                raise TypeError("args must be positional-only")
            args = positional[0]
            if type(args) is not list or any(type(arg) is not str for arg in args):
                raise TypeError("args must be list[str]")
            if mutation == "surrogate" and args == ["\udcff"]:
                raise TypeError("surrogate rejected")

        def start(self) -> ProbeAwaitable:
            return ProbeAwaitable("wrong" if mutation == "start" else None)

        def scene(self) -> ProbeAwaitable:
            if mutation == "scene":
                return ProbeAwaitable()
            return ProbeAwaitable(
                error=NotImplementedError("Production.scene() is not implemented")
            )

        def stop(self) -> ProbeAwaitable:
            return ProbeAwaitable("wrong" if mutation == "stop" else None)

    class FactoryOnly:
        gate = ""

        def __new__(cls) -> FactoryOnly:
            if mutation == f"native-{cls.gate}-construction-gate":
                return object.__new__(cls)
            raise TypeError("factory only")

    class ProbeActor(FactoryOnly):
        gate = "actor"

    class ProbeActorHandle(FactoryOnly):
        gate = "actor-handle"

    @dataclass(frozen=True, slots=True, kw_only=True)
    class ProbeAgentProfile:
        agent: str
        workspace: str
        model: str
        effort: str | None

    class ProbeAgentError(RuntimeError):
        pass

    class ProbeAgentSessionBusyError(ProbeAgentError):
        pass

    class ProbeAgentSessionError(ProbeAgentError):
        pass

    class ProbeAgentSessionStartError(ProbeAgentSessionError):
        pass

    class ProbeAgentAuthenticationRequiredError(ProbeAgentSessionStartError):
        pass

    class ProbeAgentSessionBrokenError(ProbeAgentSessionError):
        pass

    class ProbeAgentTurnError(ProbeAgentError):
        pass

    class ProbeAgentResultError(ProbeAgentTurnError):
        pass

    class ProbeAgentResultMissingError(ProbeAgentResultError):
        pass

    class ProbeAgentResultIssue:
        pass

    class ProbeCue(FactoryOnly):
        gate = "cue"

    class ProbeEffect(FactoryOnly):
        gate = "effect"

    class ProbeCueContextError(RuntimeError):
        pass

    class ProbeEffectContextError(RuntimeError):
        pass

    class ProbeSchemaValue(ABC):
        @abstractmethod
        def render_prompt(self) -> str:
            raise NotImplementedError

        @abstractmethod
        def validate(self, value: object, /) -> None:
            raise NotImplementedError

    schema_types: dict[str, type[object]] = {
        name: type(name, (), {}) for name in SCHEMA_EXPORTS
    }
    schema_types["SchemaValue"] = ProbeSchemaValue
    schema_types["ValueRejected"] = type("ValueRejected", (ValueError,), {})
    schema_types["SchemaCallbackError"] = type(
        "SchemaCallbackError",
        (RuntimeError,),
        {},
    )
    for schema_type in schema_types.values():
        schema_type.__module__ = "troupe.act_schema"
    schema = ModuleType("troupe.act_schema")
    for name, schema_type in schema_types.items():
        setattr(schema, name, schema_type)
    schema.__all__ = ["Other"] if mutation == "schema-exports" else SCHEMA_EXPORTS

    public_types = {
        "Actor": ProbeActor,
        "ActorHandle": ProbeActorHandle,
        "AgentAuthenticationRequiredError": ProbeAgentAuthenticationRequiredError,
        "AgentError": ProbeAgentError,
        "AgentProfile": ProbeAgentProfile,
        "AgentResultError": ProbeAgentResultError,
        "AgentResultIssue": ProbeAgentResultIssue,
        "AgentResultMissingError": ProbeAgentResultMissingError,
        "AgentSessionBrokenError": ProbeAgentSessionBrokenError,
        "AgentSessionBusyError": ProbeAgentSessionBusyError,
        "AgentSessionError": ProbeAgentSessionError,
        "AgentSessionStartError": ProbeAgentSessionStartError,
        "AgentTurnError": ProbeAgentTurnError,
        "Cue": ProbeCue,
        "CueContextError": ProbeCueContextError,
        "Effect": ProbeEffect,
        "EffectContextError": ProbeEffectContextError,
        "Production": ProbeProduction,
    }
    for public_type in public_types.values():
        public_type.__module__ = "troupe"
    if mutation is not None and mutation.startswith("module-"):
        module_name = mutation.removeprefix("module-")
        if module_name == "act_schema":
            schema.__name__ = "troupe._runtime.act_schema"
        else:
            public_types[module_name].__module__ = "troupe._runtime"

    package = ModuleType("troupe")
    package.__path__ = []
    package.__file__ = "/child/lib/python/site-packages/troupe/__init__.py"
    for name, public_type in public_types.items():
        setattr(package, name, public_type)
    package.act_schema = schema
    package.__all__ = ["Other"] if mutation == "exports" else PUBLIC_EXPORTS
    runtime = ModuleType("troupe._runtime")
    runtime.__file__ = "/child/lib/python/site-packages/troupe/_runtime.abi3.so"
    for name, public_type in public_types.items():
        if name != "AgentProfile":
            setattr(runtime, name, public_type)
    runtime.act_schema = schema
    if mutation == "agent-test-support":
        runtime._agent_test_set_launch = object()
    if mutation is not None and mutation.startswith("identity-"):
        setattr(runtime, mutation.removeprefix("identity-"), object())
    package._runtime = runtime
    dependency = ModuleType("troupe_smoke_dependency")
    dependency.__file__ = "/child/lib/python/site-packages/troupe_smoke_dependency.py"
    dependency.VALUE = "dependency-ok"
    monkeypatch.setitem(sys.modules, "troupe", package)
    monkeypatch.setitem(sys.modules, "troupe._runtime", runtime)
    monkeypatch.setitem(sys.modules, "troupe.act_schema", schema)
    monkeypatch.setitem(sys.modules, "troupe_smoke_dependency", dependency)

    class EntryPoint:
        name = "troupe"
        value = "troupe:main" if mutation == "entrypoint" else "troupe._runtime:main"

    monkeypatch.setattr(
        importlib.metadata,
        "entry_points",
        lambda *, group: [EntryPoint()] if group == "console_scripts" else [],
    )
    real_config_var = sysconfig.get_config_var
    monkeypatch.setattr(
        sysconfig,
        "get_config_var",
        lambda name: 1 if mutation == "gil" and name == "Py_GIL_DISABLED" else real_config_var(name),
    )

    output = io.StringIO()
    with contextlib.redirect_stdout(output):
        exec(compile(verifier.SMOKE, "<troupe-wheel-smoke>", "exec"), {})
    return json.loads(output.getvalue())


def test_child_probe_executes_and_reports_every_required_check(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    verifier = _verifier()
    payload = _execute_child_probe(verifier, monkeypatch, None)
    expected = _installed_smoke_payload(Path("/child"))
    assert payload == expected


@pytest.mark.parametrize(
    ("mutation", "error_type"),
    [
        *((f"identity-{name}", AssertionError) for name in PUBLIC_EXPORTS),
        *((f"module-{name}", AssertionError) for name in PUBLIC_EXPORTS),
        ("schema-exports", AssertionError),
        ("agent-test-support", AssertionError),
        ("native-actor-construction-gate", AssertionError),
        ("native-actor-handle-construction-gate", AssertionError),
        ("native-cue-construction-gate", AssertionError),
        ("native-effect-construction-gate", AssertionError),
        ("exports", AssertionError),
        ("gil", AssertionError),
        ("constructor", AssertionError),
        ("surrogate", TypeError),
        ("start", AssertionError),
        ("scene", AssertionError),
        ("stop", AssertionError),
        ("subclass", RuntimeError),
        ("entrypoint", AssertionError),
    ],
)
def test_child_probe_fails_when_each_runtime_fact_is_wrong(
    monkeypatch: pytest.MonkeyPatch,
    mutation: str,
    error_type: type[Exception],
) -> None:
    verifier = _verifier()
    with pytest.raises(error_type):
        _execute_child_probe(verifier, monkeypatch, mutation)


def test_smoke_event_log_accepts_every_actor_and_cancellation_fact(tmp_path: Path) -> None:
    verifier = _verifier()
    path = tmp_path / "events.json"
    raw_args = ["--events", str(path), "--value", "7", "input.txt"]
    expected = _installed_smoke_events(raw_args)
    path.write_text(json.dumps(expected), encoding="utf-8")
    verifier._validate_smoke_events(path, raw_args)


@pytest.mark.parametrize(
    "mutation",
    [
        "empty",
        "args",
        "lifecycle",
        "constructor",
        "query",
        "root-cue",
        "downstream",
        "effect",
        "result",
        "threads",
        "admission",
        "pending",
        "other-progress",
        "release-order",
        "caller-outcome",
        "successor-result",
        "stop-not-last",
        "extra",
    ],
)
def test_smoke_event_log_rejects_each_wrong_semantic_group(
    tmp_path: Path,
    mutation: str,
) -> None:
    verifier = _verifier()
    path = tmp_path / "events.json"
    raw_args = ["--events", str(path), "--value", "7", "input.txt"]
    expected = _installed_smoke_events(raw_args)
    events = json.loads(json.dumps(expected))
    actor = events[3][1]
    cancellation = events[4][1]
    assert isinstance(actor, dict)
    assert isinstance(cancellation, dict)
    if mutation == "empty":
        events = []
    elif mutation == "args":
        events[0] = ["args", ["wrong"]]
    elif mutation == "lifecycle":
        events[2] = ["scene", "wrong", "module-ok", "resource-ok"]
    elif mutation == "constructor":
        actor["constructors"][0][2] = False
    elif mutation == "query":
        actor["queries"]["pattern"] = ["worker", "router"]
    elif mutation == "root-cue":
        actor["root_cue"]["id"] = "wrong-cue0"
    elif mutation == "downstream":
        actor["downstream_cue"]["source"] = "scene-source"
    elif mutation == "effect":
        actor["effect"]["owner"] = "router"
    elif mutation == "result":
        actor["result"]["type"] = "list"
    elif mutation == "threads":
        actor["threads"].append(4243)
    elif mutation == "admission":
        cancellation["admitted_snapshot"] = "after-release"
    elif mutation == "pending":
        cancellation["pre_release"]["successor_done"] = True
    elif mutation == "other-progress":
        cancellation["other_actor_result"] = ["wrong"]
    elif mutation == "release-order":
        cancellation["completion_saw_release"]["caller"] = False
    elif mutation == "caller-outcome":
        cancellation["caller_outcome"] = "success"
    elif mutation == "successor-result":
        cancellation["successor_result"] = ["wrong"]
    elif mutation == "stop-not-last":
        events[4], events[5] = events[5], events[4]
    elif mutation == "extra":
        events.append(["extra"])
    else:
        raise AssertionError(f"unknown mutation: {mutation}")
    path.write_text(json.dumps(events), encoding="utf-8")
    with pytest.raises(verifier.VerificationError):
        verifier._validate_smoke_events(path, raw_args)


def test_smoke_event_log_rejects_missing_or_invalid_json(tmp_path: Path) -> None:
    verifier = _verifier()
    path = tmp_path / "events.json"
    with pytest.raises(verifier.VerificationError):
        verifier._validate_smoke_events(path, [])
    path.write_text("not json", encoding="utf-8")
    with pytest.raises(verifier.VerificationError):
        verifier._validate_smoke_events(path, [])


def test_installed_smoke_event_producers_derive_the_validated_facts(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    production = _load_wheel_smoke_production(monkeypatch)
    actor, cancellation = _producer_observations()
    expected = _installed_smoke_events([])

    assert production._actor_round_trip_event(**actor) == expected[3]
    assert production._cancellation_event(**cancellation) == expected[4]


def test_installed_smoke_event_producers_reject_each_wrong_observation(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    production = _load_wheel_smoke_production(monkeypatch)
    actor_mutations = (
        "router-handle",
        "worker-handle",
        "constructor",
        "exact-query",
        "pattern-query",
        "root-cue",
        "downstream-cue",
        "effect",
        "result-type",
        "threads",
    )
    for mutation in actor_mutations:
        actor, _ = _producer_observations()
        if mutation == "router-handle":
            actor["router"].name = "wrong"
        elif mutation == "worker-handle":
            actor["worker"].name = "wrong"
        elif mutation == "constructor":
            actor["constructors"][0][2] = False
        elif mutation == "exact-query":
            actor["exact"].name = "wrong"
        elif mutation == "pattern-query":
            actor["pattern"].reverse()
        elif mutation == "root-cue":
            actor["root_cue"].source = "wrong"
        elif mutation == "downstream-cue":
            actor["downstream_cue"].source = "wrong"
        elif mutation == "effect":
            actor["result"][0].owner = "router"
        elif mutation == "result-type":
            actor["result"] = list(actor["result"])
        elif mutation == "threads":
            actor["threads"].append(4243)
        with pytest.raises(AssertionError):
            production._actor_round_trip_event(**actor)

    cancellation_mutations = (
        "admission",
        "caller-pending",
        "successor-pending",
        "successor-entry",
        "other-progress",
        "caller-release-order",
        "successor-release-order",
        "caller-outcome",
        "successor-result",
    )
    for mutation in cancellation_mutations:
        _, cancellation = _producer_observations()
        if mutation == "admission":
            cancellation["admitted_snapshot"] = "after-release"
        elif mutation == "caller-pending":
            cancellation["caller_done"] = True
        elif mutation == "successor-pending":
            cancellation["successor_done"] = True
        elif mutation == "successor-entry":
            cancellation["successor_entered"] = True
        elif mutation == "other-progress":
            cancellation["other_actor_result"] = ("wrong",)
        elif mutation == "caller-release-order":
            cancellation["caller_completion_saw_release"] = False
        elif mutation == "successor-release-order":
            cancellation["successor_completion_saw_release"] = False
        elif mutation == "caller-outcome":
            cancellation["caller_outcome"] = "success"
        elif mutation == "successor-result":
            cancellation["successor_result"] = ("wrong",)
        with pytest.raises(AssertionError):
            production._cancellation_event(**cancellation)


def test_installed_smoke_fixture_uses_only_public_actor_api_and_bounded_waits() -> None:
    fixture = (
        ROOT
        / "tests"
        / "fixtures"
        / "productions"
        / "wheel_smoke_production"
        / "production.py"
    )
    source = fixture.read_text(encoding="utf-8")
    tree = ast.parse(source)

    troupe_imports = [
        node
        for node in tree.body
        if isinstance(node, ast.Import)
        and [(alias.name, alias.asname) for alias in node.names] == [("troupe", None)]
    ]
    assert len(troupe_imports) == 1
    for node in ast.walk(tree):
        if isinstance(node, ast.Import):
            for alias in node.names:
                assert not _is_module_path(alias.name, "tests")
                assert not _is_module_path(alias.name, "time")
                if _is_module_path(alias.name, "troupe"):
                    assert alias.name == "troupe"
        elif isinstance(node, ast.ImportFrom):
            assert all(alias.name != "*" for alias in node.names)
            module = node.module or ""
            assert not _is_module_path(module, "tests")
            assert not _is_module_path(module, "time")
            assert not _is_module_path(module, "troupe")

    allowed_asyncio = {
        "asyncio.CancelledError",
        "asyncio.Event",
        "asyncio.create_task",
        "asyncio.get_running_loop",
        "asyncio.wait_for",
    }
    for node in ast.walk(tree):
        if not isinstance(node, ast.Attribute):
            continue
        dotted = _dotted_name(node)
        if dotted is None:
            continue
        if dotted.startswith("troupe."):
            public_name = dotted.split(".", 2)[1]
            assert public_name in PUBLIC_EXPORTS
        elif dotted.startswith("asyncio."):
            assert dotted in allowed_asyncio
        elif dotted.startswith("re."):
            assert dotted == "re.compile"
        elif dotted.startswith("threading."):
            assert dotted == "threading.get_ident"

    classes = [node for node in tree.body if isinstance(node, ast.ClassDef)]
    bases = [
        _dotted_name(base)
        for definition in classes
        for base in definition.bases
    ]
    assert bases.count("troupe.Actor") >= 2
    assert bases.count("troupe.Effect") >= 1
    assert bases.count("troupe.Production") == 1
    production_class = next(
        definition
        for definition in classes
        if [_dotted_name(base) for base in definition.bases] == ["troupe.Production"]
    )
    assert any(
        isinstance(node, ast.AsyncFunctionDef) and node.name == "scene"
        for node in production_class.body
    )
    actor_classes = [
        definition
        for definition in classes
        if [_dotted_name(base) for base in definition.bases] == ["troupe.Actor"]
    ]
    assert all(
        any(
            isinstance(node, ast.AsyncFunctionDef) and node.name == "cued"
            for node in definition.body
        )
        for definition in actor_classes
    )

    calls = [node for node in ast.walk(tree) if isinstance(node, ast.Call)]
    call_names = [_dotted_name(call.func) for call in calls]
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
    assert sum(
        isinstance(call.func, ast.Attribute) and call.func.attr == "get_actor"
        for call in calls
    ) >= 2
    assert sum(
        isinstance(call.func, ast.Attribute) and call.func.attr == "cue"
        for call in calls
    ) >= 3
    assert any(
        isinstance(call.func, ast.Attribute) and call.func.attr == "make_effect"
        for call in calls
    )
    assert call_names.count("asyncio.create_task") >= 2
    assert "re.compile" in call_names
    assert "threading.get_ident" in call_names

    wait_for_calls = [
        call for call in calls if _dotted_name(call.func) == "asyncio.wait_for"
    ]
    assert wait_for_calls
    assert all(
        len(call.args) >= 2
        or any(keyword.arg == "timeout" for keyword in call.keywords)
        for call in wait_for_calls
    )
    assert not any(
        _dotted_name(call.func) == "sleep"
        or (_dotted_name(call.func) or "").endswith(".sleep")
        for call in calls
    )
@pytest.mark.parametrize(
    ("uv_found", "wrong_troupe"),
    [
        (True, False),
        (False, True),
    ],
)
def test_smoke_tool_resolution_rejects_uv_or_the_wrong_console(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
    uv_found: bool,
    wrong_troupe: bool,
) -> None:
    verifier = _verifier()
    child = tmp_path / "child"
    expected_troupe = str(child / "bin" / "troupe")

    def which(name: str, *, path: str) -> str | None:
        assert path == f"{child}/bin:/usr/bin:/bin"
        if name == "uv":
            return "/usr/bin/uv" if uv_found else None
        return "/other/bin/troupe" if wrong_troupe else expected_troupe

    monkeypatch.setattr(verifier.shutil, "which", which)
    with pytest.raises(verifier.VerificationError):
        verifier._validate_smoke_tools(child, {"PATH": f"{child}/bin:/usr/bin:/bin"})


def test_run_rejects_a_forbidden_stderr_marker_even_on_zero_exit(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    verifier = _verifier()

    class Completed:
        returncode = 0
        stdout = ""
        stderr = "troupe: failed to run production\n"

    monkeypatch.setattr(verifier.subprocess, "run", lambda *_, **__: Completed())
    with pytest.raises(verifier.VerificationError):
        verifier._run(
            ["troupe", "--help"],
            cwd=tmp_path,
            env={},
            forbidden_stderr="troupe:",
        )


def test_mock_agent_cleanup_requires_complete_sessions_and_reaped_processes(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    verifier = _verifier()
    missing = tmp_path / "missing.jsonl"
    with pytest.raises(verifier.VerificationError, match="did not start"):
        verifier._validate_mock_agent_cleanup(missing)

    events = tmp_path / "events.jsonl"
    events.write_text(_mock_agent_events(), encoding="utf-8")
    inspected: list[int] = []

    def reaped(pid: int, signal: int) -> None:
        assert signal == 0
        inspected.append(pid)
        raise ProcessLookupError

    monkeypatch.setattr(verifier.os, "kill", reaped)
    verifier._validate_mock_agent_cleanup(events)
    assert set(inspected) == {101, 102}

    monkeypatch.setattr(verifier.os, "kill", lambda _pid, _signal: None)
    with pytest.raises(verifier.VerificationError, match="left a mock agent"):
        verifier._validate_mock_agent_cleanup(events)


def test_run_wraps_a_bounded_smoke_timeout_after_subprocess_reaping(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    verifier = _verifier()
    timeout_seconds = 7.5
    timeout = subprocess.TimeoutExpired(["troupe"], timeout_seconds)
    calls: list[dict[str, object]] = []

    class Completed:
        returncode = 0
        stdout = ""
        stderr = ""

    def run(*_: object, **kwargs: object) -> Completed:
        calls.append(dict(kwargs))
        if kwargs.get("timeout") == timeout_seconds:
            raise timeout
        return Completed()

    monkeypatch.setattr(verifier.subprocess, "run", run)

    with pytest.raises(verifier.VerificationError, match="timed out"):
        verifier._run(
            ["troupe"],
            cwd=tmp_path,
            env={},
            timeout=timeout_seconds,
        )
    assert len(calls) == 1
    assert calls[0]["timeout"] == timeout_seconds


def test_clean_smoke_wiring_uses_child_python_offline_and_literal_console(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    verifier = _verifier()
    assert 0 < verifier.SMOKE_TIMEOUT <= 60
    wheel = tmp_path / "troupe.whl"
    wheel.touch()
    workspace = tmp_path / "smoke-workspace"
    workspace.mkdir()
    child = workspace / "child-venv"
    outside = workspace / "outside-repository"
    events_path = workspace / "events.json"
    fixture = ROOT / "tests" / "fixtures" / "productions" / "wheel_smoke_production"
    raw_args = ["--events", str(events_path), "--value", "7", "input.txt"]
    expected_events = _installed_smoke_events(raw_args)
    managed_python = tmp_path / "managed" / "bin" / "python3.10"
    managed_python.parent.mkdir(parents=True)
    managed_python.touch()
    project_python = tmp_path / "project" / "bin" / "python"
    project_python.parent.mkdir(parents=True)
    project_python.symlink_to(managed_python)
    original_base_executable = str(project_python)
    resolved_base_executable = str(managed_python.resolve())
    monkeypatch.setattr(verifier.sys, "_base_executable", original_base_executable)
    calls: list[tuple[list[str], Path, dict[str, str], dict[str, object]]] = []
    which_calls: list[tuple[str, str]] = []
    timeline: list[str] = []
    builder_flags: list[bool] = []
    builder_init_base_executables: list[str] = []
    builder_base_executables: list[str] = []
    post_create_base_executables: list[str] = []

    class FakeEnvBuilder:
        def __init__(self, *, with_pip: bool) -> None:
            builder_flags.append(with_pip)
            builder_init_base_executables.append(verifier.sys._base_executable)

        def create(self, path: Path) -> None:
            builder_base_executables.append(verifier.sys._base_executable)
            bin_dir = path / "bin"
            bin_dir.mkdir(parents=True)
            for name in ("python", "troupe"):
                executable = bin_dir / name
                executable.write_text("#!/bin/sh\nexit 0\n", encoding="utf-8")
                executable.chmod(0o755)
            payload = _installed_smoke_payload(path)
            for key in ("troupe_file", "runtime_file", "dependency_file"):
                installed_file = Path(str(payload[key]))
                installed_file.parent.mkdir(parents=True, exist_ok=True)
                installed_file.touch()

    monkeypatch.setattr(verifier.venv, "EnvBuilder", FakeEnvBuilder)
    dependency = workspace / "dependency.whl"
    dependency.touch()

    def build_dependency(_: Path) -> Path:
        post_create_base_executables.append(verifier.sys._base_executable)
        return dependency

    monkeypatch.setattr(verifier, "_build_dependency_wheel", build_dependency)

    child_python = str(child / "bin" / "python")
    expected_commands = [
        [
            child_python,
            "-m",
            "pip",
            "install",
            "--no-index",
            "--no-deps",
            str(wheel.resolve()),
            str(dependency.resolve()),
        ],
        [child_python, "-m", "pip", "check"],
        [child_python, "-c", verifier.SMOKE],
        ["troupe", "--help"],
        ["troupe", "--production", str(fixture), "--", *raw_args],
    ]

    def run(
        command: list[str],
        *,
        cwd: Path,
        env: dict[str, str],
        **kwargs: object,
    ) -> str:
        index = len(calls)
        assert command == expected_commands[index]
        timeline.append(f"run-{index}")
        calls.append((command, cwd, dict(env), dict(kwargs)))
        if index == 2:
            return json.dumps(_installed_smoke_payload(child))
        if index == 4:
            events_path.write_text(json.dumps(expected_events), encoding="utf-8")
            (workspace / "agent-events.jsonl").write_text(
                _mock_agent_events(),
                encoding="utf-8",
            )
        return ""

    monkeypatch.setattr(verifier, "_run", run)
    monkeypatch.setattr(
        verifier.os,
        "kill",
        lambda _pid, _signal: (_ for _ in ()).throw(ProcessLookupError()),
    )
    real_which = shutil.which

    def which(name: str, *, path: str) -> str | None:
        timeline.append(f"which-{name}")
        which_calls.append((name, path))
        return real_which(name, path=path)

    monkeypatch.setattr(verifier.shutil, "which", which)
    validations = {
        "tools": 0,
        "payload": 0,
        "paths": 0,
        "events": 0,
        "agent_cleanup": 0,
    }
    for name, key in (
        ("_validate_smoke_tools", "tools"),
        ("_validate_smoke_payload", "payload"),
        ("_validate_installed_paths", "paths"),
        ("_validate_smoke_events", "events"),
        ("_validate_mock_agent_cleanup", "agent_cleanup"),
    ):
        real = getattr(verifier, name)

        def recording(*args: object, _real=real, _key=key, **kwargs: object):
            timeline.append(f"validate-{_key}")
            validations[_key] += 1
            return _real(*args, **kwargs)

        monkeypatch.setattr(verifier, name, recording)

    for name in ("CONDA_PREFIX", "PYTHONPATH", "PYTHONHOME", "VIRTUAL_ENV"):
        monkeypatch.setenv(name, f"/{name.lower()}")
    monkeypatch.delenv("PYTHONDONTWRITEBYTECODE", raising=False)

    verifier._smoke_wheel(wheel, workspace)

    assert builder_flags == [True]
    assert builder_init_base_executables == [original_base_executable]
    assert builder_base_executables == [resolved_base_executable]
    assert post_create_base_executables == [original_base_executable]
    assert verifier.sys._base_executable == original_base_executable
    launcher = child / "bin" / "npx"
    assert launcher.stat().st_mode & 0o111
    launcher_source = launcher.read_text(encoding="utf-8")
    assert str(child / "bin" / "python") in launcher_source
    assert str(ROOT / "tests" / "support" / "mock_acp_agent.py") in launcher_source
    assert len(calls) == 5
    expected_path = f"{child}/bin:/usr/bin:/bin"
    assert which_calls == [("uv", expected_path), ("troupe", expected_path)]
    assert validations == {
        "tools": 1,
        "payload": 1,
        "paths": 1,
        "events": 1,
        "agent_cleanup": 1,
    }
    assert timeline == [
        "run-0",
        "run-1",
        "run-2",
        "validate-payload",
        "validate-paths",
        "validate-tools",
        "which-uv",
        "which-troupe",
        "run-3",
        "run-4",
        "validate-events",
        "validate-agent_cleanup",
    ]
    for index, (_, cwd, env, kwargs) in enumerate(calls):
        assert cwd == outside
        assert env["PATH"] == expected_path
        assert {
            "CONDA_PREFIX",
            "PYTHONPATH",
            "PYTHONHOME",
            "VIRTUAL_ENV",
        }.isdisjoint(env)
        assert env["PYTHONDONTWRITEBYTECODE"] == "1"
        expected_kwargs: dict[str, object] = {}
        if index in (2, 4):
            expected_kwargs["timeout"] = verifier.SMOKE_TIMEOUT
        if index in (3, 4):
            expected_kwargs["forbidden_stderr"] = "troupe:"
        assert kwargs == expected_kwargs


@pytest.mark.parametrize("failure", ["os-error", "called-process-error", "abort"])
def test_clean_smoke_restores_base_executable_when_venv_creation_fails(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
    failure: str,
) -> None:
    verifier = _verifier()
    wheel = tmp_path / "troupe.whl"
    wheel.touch()
    workspace = tmp_path / "smoke-workspace"
    workspace.mkdir()
    managed_python = tmp_path / "managed" / "bin" / "python3.10"
    managed_python.parent.mkdir(parents=True)
    managed_python.touch()
    project_python = tmp_path / "project" / "bin" / "python"
    project_python.parent.mkdir(parents=True)
    project_python.symlink_to(managed_python)
    original_base_executable = str(project_python)
    resolved_base_executable = str(managed_python.resolve())
    monkeypatch.setattr(verifier.sys, "_base_executable", original_base_executable)
    builder_init_base_executables: list[str] = []
    builder_base_executables: list[str] = []

    class BuilderAbort(BaseException):
        pass

    errors: dict[str, BaseException] = {
        "os-error": OSError("venv creation failed"),
        "called-process-error": subprocess.CalledProcessError(1, ["ensurepip"]),
        "abort": BuilderAbort("venv creation aborted"),
    }
    builder_error = errors[failure]

    class FailingEnvBuilder:
        def __init__(self, *, with_pip: bool) -> None:
            assert with_pip is True
            builder_init_base_executables.append(verifier.sys._base_executable)

        def create(self, _: Path) -> None:
            builder_base_executables.append(verifier.sys._base_executable)
            raise builder_error

    monkeypatch.setattr(verifier.venv, "EnvBuilder", FailingEnvBuilder)

    if failure == "os-error":
        with pytest.raises(verifier.VerificationError, match="could not create child venv"):
            verifier._smoke_wheel(wheel, workspace)
    else:
        with pytest.raises(type(builder_error)) as caught:
            verifier._smoke_wheel(wheel, workspace)
        assert caught.value is builder_error

    assert builder_init_base_executables == [original_base_executable]
    assert builder_base_executables == [resolved_base_executable]
    assert verifier.sys._base_executable == original_base_executable
