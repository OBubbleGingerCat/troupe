from __future__ import annotations

import importlib
from pathlib import Path
from typing import Any

try:
    import tomllib
except ModuleNotFoundError:  # pragma: no cover - Python 3.10
    import tomli as tomllib


ROOT = Path(__file__).resolve().parents[2]


def _native() -> Any:
    return importlib.import_module("troupe._runtime")


def test_mcp_official_acp_dependency_pins_the_sdk_not_an_unstable_transport() -> None:
    with (ROOT / "rust" / "Cargo.toml").open("rb") as handle:
        manifest = tomllib.load(handle)

    assert manifest["dependencies"]["agent-client-protocol"] == {
        "version": "=2.0.0"
    }
    assert manifest["dependencies"]["getrandom"] == "0.4"
    assert manifest["features"] == {
        "default": [],
        "agent-test-support": [],
    }


def test_ready_private_adapter_registry_is_closed_and_exact() -> None:
    snapshot = _native()._agent_launch_specs_for_test()

    assert snapshot == {
        "codex": {
            "program": "npx",
            "args": ["--yes", "@agentclientprotocol/codex-acp@1.1.9"],
            "version": "1.1.9",
            "acp_wire_protocol": "stable-v1",
            "client_sdk_version": "2.0.0",
            "mcp_wire_protocol": "2025-11-25",
            "mcp_transport_profile": "McpTransportProfileV1",
            "environment_policy": "inherit_parent",
            "fixed_environment": {"INITIAL_AGENT_MODE": "agent"},
            "initial_mode": "agent",
            "mode_application": {
                "method": "session/set_config_option",
                "config_id": "mode",
                "value": "agent",
            },
            "model_config_id": "model",
            "effort_config_id": "reasoning_effort",
            "configuration_order": ["mode", "model", "effort"],
            "effective_value_validation": "exact_advertised_select",
            "mcp_registration": "session/new.mcpServers.http",
            "autonomous_request_profile": "codex-acp@1.1.9",
            "settlement_profile": "codex-acp@1.1.9",
        },
        "claude": {
            "program": "npx",
            "args": ["--yes", "@agentclientprotocol/claude-agent-acp@0.64.2"],
            "version": "0.64.2",
            "acp_wire_protocol": "stable-v1",
            "client_sdk_version": "2.0.0",
            "mcp_wire_protocol": "2025-11-25",
            "mcp_transport_profile": "McpTransportProfileV1",
            "environment_policy": "inherit_parent",
            "fixed_environment": {},
            "initial_mode": "default",
            "mode_application": {
                "method": "session/set_config_option",
                "config_id": "mode",
                "value": "default",
            },
            "model_config_id": "model",
            "effort_config_id": "effort",
            "configuration_order": ["mode", "model", "effort"],
            "effective_value_validation": "exact_advertised_select",
            "mcp_registration": "session/new.mcpServers.http",
            "autonomous_request_profile": "claude-agent-acp@0.64.2",
            "settlement_profile": "claude-agent-acp@0.64.2",
        },
        "kimi": {
            "program": "kimi",
            "args": ["acp"],
            "version": "0.31.1",
            "acp_wire_protocol": "stable-v1",
            "client_sdk_version": "2.0.0",
            "mcp_wire_protocol": "2025-11-25",
            "mcp_transport_profile": "McpTransportProfileV1",
            "environment_policy": "inherit_parent",
            "fixed_environment": {},
            "initial_mode": "default",
            "mode_application": {
                "method": "session/set_config_option",
                "config_id": "mode",
                "value": "default",
            },
            "model_config_id": "model",
            "effort_config_id": "thinking",
            "configuration_order": ["mode", "model", "effort"],
            "effective_value_validation": "exact_advertised_select",
            "mcp_registration": "session/new.mcpServers.http",
            "autonomous_request_profile": "kimi-code@0.31.1",
            "settlement_profile": "kimi-code@0.31.1",
        },
    }
    assert "latest" not in repr(snapshot).lower()


def test_cast_default_public_module_has_no_adapter_override() -> None:
    import troupe

    assert not hasattr(troupe, "AgentAdapter")
    assert not hasattr(troupe, "NpxAgent")
    assert not hasattr(troupe.AgentProfile, "command")
    assert not hasattr(troupe.AgentProfile, "version")
