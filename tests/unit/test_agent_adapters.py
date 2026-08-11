from __future__ import annotations

import importlib
import importlib.util
import json
import os
from pathlib import Path
from typing import Any

import pytest

try:
    import tomllib
except ModuleNotFoundError:  # pragma: no cover - Python 3.10
    import tomli as tomllib


ROOT = Path(__file__).resolve().parents[2]


def _native() -> Any:
    return importlib.import_module("troupe._runtime")


def _live_acceptance_module() -> Any:
    path = ROOT / "tests" / "live" / "provider_acceptance.py"
    spec = importlib.util.spec_from_file_location("_troupe_provider_acceptance", path)
    assert spec is not None
    assert spec.loader is not None
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


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
            "mcp_wire_protocol": "2025-06-18",
            "mcp_transport_profile": "McpTransportProfileV1",
            "environment_policy": "inherit_parent",
            "fixed_environment": {"INITIAL_AGENT_MODE": "agent"},
            "removed_environment": ["CODEX_PATH"],
            "initial_mode": "agent",
            "mode_application": {
                "method": "session/set_config_option",
                "config_id": "mode",
                "value": "agent",
            },
            "model_config_id": "model",
            "effort_config_id": "reasoning_effort",
            "effort_option_optional_when_unspecified": False,
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
            "removed_environment": [],
            "initial_mode": "default",
            "mode_application": {
                "method": "session/set_config_option",
                "config_id": "mode",
                "value": "default",
            },
            "model_config_id": "model",
            "effort_config_id": "effort",
            "effort_option_optional_when_unspecified": True,
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
            "removed_environment": [],
            "initial_mode": "default",
            "mode_application": {
                "method": "session/set_config_option",
                "config_id": "mode",
                "value": "default",
            },
            "model_config_id": "model",
            "effort_config_id": "thinking",
            "effort_option_optional_when_unspecified": False,
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


def _permission_request(
    *,
    options: list[dict[str, object]],
    kind: str = "execute",
    meta: dict[str, object] | None = None,
) -> str:
    request: dict[str, object] = {
        "sessionId": "codex-session",
        "toolCall": {
            "toolCallId": "codex-tool-call",
            "kind": kind,
            "status": "pending",
        },
        "options": options,
    }
    if meta is not None:
        request["_meta"] = meta
    return json.dumps(request)


def _option(
    option_id: str,
    kind: str,
    *,
    meta: dict[str, object] | None = None,
) -> dict[str, object]:
    option: dict[str, object] = {
        "optionId": option_id,
        "name": f"display text for {option_id}",
        "kind": kind,
    }
    if meta is not None:
        option["_meta"] = meta
    return option


@pytest.mark.parametrize(
    ("request_json", "expected"),
    [
        (
            _permission_request(
                options=[
                    _option(
                        "reject_once",
                        "reject_once",
                        meta={"codex": {"decision": "decline"}},
                    ),
                    _option(
                        "allow_once",
                        "allow_once",
                        meta={"codex": {"decision": "accept"}},
                    ),
                    _option(
                        "allow_always",
                        "allow_always",
                        meta={"codex": {"decision": "acceptForSession"}},
                    ),
                ],
                meta={"codex": {"params": {"itemId": "command-item"}}},
            ),
            "selected:allow_once",
        ),
        (
            _permission_request(
                kind="other",
                options=[
                    _option(
                        "allow_permissions_session",
                        "allow_always",
                        meta={
                            "codex": {"decision": "allowPermissionsForSession"}
                        },
                    ),
                    _option(
                        "allow_permissions_turn",
                        "allow_once",
                        meta={"codex": {"decision": "allowPermissionsForTurn"}},
                    ),
                    _option(
                        "reject_permissions",
                        "reject_once",
                        meta={"codex": {"decision": "rejectPermissions"}},
                    ),
                ],
                meta={"codex": {"params": {"itemId": "permission-item"}}},
            ),
            "selected:allow_permissions_turn",
        ),
        (
            _permission_request(
                options=[
                    _option("decline", "reject_once"),
                    _option("allow_once", "allow_once"),
                    _option("allow_session", "allow_always"),
                ],
                meta={"is_mcp_tool_approval": True},
            ),
            "selected:allow_once",
        ),
        (
            _permission_request(
                kind="switch_mode",
                options=[
                    _option("revise_plan", "reject_once"),
                    _option("implement_plan", "allow_once"),
                ],
                meta={
                    "codex": {
                        "kind": "plan_review",
                        "planItemId": "plan-item",
                    }
                },
            ),
            "selected:implement_plan",
        ),
        (
            _permission_request(
                kind="other",
                options=[
                    _option("accept", "allow_once"),
                    _option("decline", "reject_once"),
                ],
            ),
            "selected:decline",
        ),
        (
            _permission_request(
                options=[
                    _option("unknown-allow", "allow_once"),
                    _option("unknown-reject", "reject_once"),
                ],
                meta={"future_adapter_shape": True},
            ),
            "selected:unknown-reject",
        ),
        (
            _permission_request(
                options=[
                    _option("reject-a", "reject_once"),
                    _option("reject-b", "reject_once"),
                ],
                meta={"future_adapter_shape": True},
            ),
            "cancelled",
        ),
    ],
)
def test_codex_adapter_resolves_autonomous_requests_without_display_text_or_order(
    request_json: str,
    expected: str,
) -> None:
    assert (
        _native()._agent_adapter_permission_for_test("codex", request_json)
        == expected
    )


@pytest.mark.parametrize(
    ("code", "data", "expected"),
    [
        (-32000, None, "authentication_lost"),
        (
            -32603,
            {"codexErrorInfo": "usageLimitExceeded"},
            "authoritative_request_failure",
        ),
        (
            -32603,
            {"codexErrorInfo": "unauthorized"},
            "authentication_lost",
        ),
        (
            -32603,
            {
                "codexErrorInfo": {
                    "responseStreamConnectionFailed": {"httpStatusCode": 401}
                }
            },
            "authentication_lost",
        ),
        (-32603, None, "uncertain"),
        (1001, None, "uncertain"),
    ],
)
def test_codex_adapter_maps_only_pinned_terminal_error_evidence(
    code: int,
    data: object | None,
    expected: str,
) -> None:
    assert (
        _native()._agent_adapter_settlement_for_test(
            "codex", code, json.dumps(data)
        )
        == expected
    )


@pytest.mark.parametrize(
    ("request_json", "expected"),
    [
        (
            _permission_request(
                options=[
                    _option("allow_always", "allow_always"),
                    _option("reject", "reject_once"),
                    _option("allow", "allow_once"),
                ],
            ),
            "selected:allow",
        ),
        (
            _permission_request(
                kind="switch_mode",
                options=[
                    _option("acceptEdits", "allow_always"),
                    _option("plan", "reject_once"),
                    _option("default", "allow_once"),
                ],
            ),
            "selected:default",
        ),
        (
            _permission_request(
                options=[
                    _option("future-allow", "allow_once"),
                    _option("future-reject", "reject_once"),
                ],
                meta={"future_adapter_shape": True},
            ),
            "selected:future-reject",
        ),
        (
            _permission_request(
                options=[
                    _option("reject-a", "reject_once"),
                    _option("reject-b", "reject_once"),
                ],
                meta={"future_adapter_shape": True},
            ),
            "cancelled",
        ),
    ],
)
def test_claude_adapter_resolves_only_pinned_autonomous_permissions(
    request_json: str,
    expected: str,
) -> None:
    assert (
        _native()._agent_adapter_permission_for_test("claude", request_json)
        == expected
    )


@pytest.mark.parametrize(
    ("code", "data", "expected"),
    [
        (-32000, None, "authentication_lost"),
        (-32603, {"errorKind": "authentication_failed"}, "authentication_lost"),
        (-32603, {"errorKind": "oauth_org_not_allowed"}, "authentication_lost"),
        (-32603, {"errorKind": "billing_error"}, "authoritative_request_failure"),
        (-32603, {"errorKind": "rate_limit"}, "authoritative_request_failure"),
        (-32603, {"errorKind": "overloaded"}, "authoritative_request_failure"),
        (-32603, {"errorKind": "invalid_request"}, "authoritative_request_failure"),
        (-32603, {"errorKind": "model_not_found"}, "authoritative_request_failure"),
        (-32603, {"errorKind": "server_error"}, "authoritative_request_failure"),
        (-32603, {"errorKind": "unknown"}, "authoritative_request_failure"),
        (-32603, {"errorKind": "max_output_tokens"}, "authoritative_request_failure"),
        (-32603, {"errorKind": "no_result"}, "authoritative_request_failure"),
        (-32603, {"errorKind": "future_kind"}, "uncertain"),
        (-32603, None, "uncertain"),
        (-32602, {"errorKind": "rate_limit"}, "uncertain"),
    ],
)
def test_claude_adapter_maps_only_pinned_terminal_error_evidence(
    code: int,
    data: object | None,
    expected: str,
) -> None:
    assert (
        _native()._agent_adapter_settlement_for_test(
            "claude", code, json.dumps(data)
        )
        == expected
    )


@pytest.mark.parametrize(
    ("stop_reason", "expected"),
    [
        ("cancelled", "uncertain"),
        ("end_turn", "authoritative"),
    ],
)
def test_claude_adapter_does_not_treat_synthetic_cancel_as_healthy(
    stop_reason: str,
    expected: str,
) -> None:
    assert (
        _native()._agent_adapter_supervisor_response_for_test(
            "claude", stop_reason
        )
        == expected
    )


def test_codex_live_example_and_explicit_acceptance_runner_are_wired() -> None:
    expected = [
        ROOT / "examples" / "live_agents" / "README.md",
        ROOT / "examples" / "live_agents" / "codex_actor" / "__init__.py",
        ROOT / "examples" / "live_agents" / "codex_actor" / "production.py",
        ROOT / "tests" / "live" / "provider_acceptance.py",
        ROOT / "scripts" / "test_live_agent.sh",
    ]
    assert all(path.is_file() for path in expected)

    runner = expected[-1]
    assert os.access(runner, os.X_OK)
    runner_source = runner.read_text(encoding="utf-8")
    assert "provider_acceptance.py" in runner_source
    assert "codex" in runner_source
    assert "maturin build" in runner_source
    assert "uv pip install --offline --no-deps" in runner_source
    assert "mktemp -d" in runner_source
    assert "agent-test-support" not in runner_source

    production_source = expected[2].read_text(encoding="utf-8")
    assert "TROUPE_LIVE_CODEX_PROFILE" in production_source
    assert "ValueRejected" in production_source
    assert "asyncio.CancelledError" in production_source

    harness_source = expected[3].read_text(encoding="utf-8")
    assert "TROUPE_LIVE_CODEX_PROFILE" in harness_source
    assert "start_new_session=True" in harness_source


def test_claude_live_example_and_isolated_acceptance_runner_are_wired() -> None:
    expected = [
        ROOT / "examples" / "live_agents" / "README.md",
        ROOT / "examples" / "live_agents" / "claude_actor" / "__init__.py",
        ROOT / "examples" / "live_agents" / "claude_actor" / "production.py",
        ROOT / "tests" / "live" / "provider_acceptance.py",
        ROOT / "scripts" / "test_live_agent.sh",
    ]
    assert all(path.is_file() for path in expected)

    runner_source = expected[-1].read_text(encoding="utf-8")
    assert "claude" in runner_source
    assert "setproxy" not in runner_source

    production_source = expected[2].read_text(encoding="utf-8")
    assert "TROUPE_LIVE_CLAUDE_PROFILE" in production_source
    assert "ValueRejected" in production_source
    assert "asyncio.CancelledError" in production_source
    assert "choices=[self.seed_token]" not in production_source

    harness_source = expected[3].read_text(encoding="utf-8")
    assert '"--ro-bind"' in harness_source
    assert '"PostToolUse"' in harness_source
    assert '"user", "project", "local"' in harness_source
    assert "start_new_session=True" in harness_source
    assert "setproxy" not in harness_source


def test_claude_live_user_settings_fixture_does_not_inherit_ambient_settings() -> None:
    acceptance = _live_acceptance_module()
    fixture = {
        "env": {"TROUPE_CLAUDE_SETTING_PRECEDENCE": "user"},
        "hooks": {"PostToolUse": []},
    }

    isolated = acceptance._isolated_claude_user_settings(
        json.dumps(
            {
                "env": {
                    "AMBIENT_SETTING": "must-not-leak",
                    "ANTHROPIC_AUTH_TOKEN": "existing-login",
                    "ANTHROPIC_BASE_URL": "https://configured.example.invalid",
                },
                "enabledPlugins": {"ambient-plugin": True},
            }
        ).encode(),
        fixture,
    )

    assert isolated == {
        "env": {
            "ANTHROPIC_AUTH_TOKEN": "existing-login",
            "ANTHROPIC_BASE_URL": "https://configured.example.invalid",
            "TROUPE_CLAUDE_SETTING_PRECEDENCE": "user",
        },
        "hooks": {"PostToolUse": []},
    }
    assert "enabledPlugins" not in isolated
    assert "AMBIENT_SETTING" not in isolated["env"]


def test_claude_live_acceptance_requires_both_schema_repair_sequences() -> None:
    acceptance = _live_acceptance_module()
    audit = [
        {
            "hook_event_name": "PostToolUseFailure",
            "tool_name": "mcp__troupe_1__troupe_submit_result",
            "tool_input": {
                "value": {"status": "needs-human", "token": "ctx-token"}
            },
            "error": "result validation failed (invalid call 1/8)",
        },
        {
            "hook_event_name": "PostToolUse",
            "tool_name": "mcp__troupe_1__troupe_submit_result",
            "tool_input": {"value": {"status": "stored", "token": "ctx-token"}},
            "tool_response": [{"type": "text", "text": "result accepted"}],
        },
        {
            "hook_event_name": "PostToolUseFailure",
            "tool_name": "mcp__troupe_1__troupe_submit_result",
            "tool_input": {"value": {"token": "ctx-token", "confidence": 6}},
            "error": "result validation failed (invalid call 1/8)",
        },
        {
            "hook_event_name": "PostToolUse",
            "tool_name": "mcp__troupe_1__troupe_submit_result",
            "tool_input": {"value": {"token": "ctx-token", "confidence": 8}},
            "tool_response": [{"type": "text", "text": "result accepted"}],
        },
    ]

    acceptance._require_claude_schema_corrections(audit, seed_token="ctx-token")


@pytest.mark.parametrize("missing_index", range(4))
def test_claude_live_acceptance_rejects_incomplete_schema_repair_evidence(
    missing_index: int,
) -> None:
    acceptance = _live_acceptance_module()
    audit = [
        {
            "hook_event_name": "PostToolUseFailure",
            "tool_name": "mcp__troupe_1__troupe_submit_result",
            "tool_input": {
                "value": {"status": "needs-human", "token": "ctx-token"}
            },
            "error": "result validation failed (invalid call 1/8)",
        },
        {
            "hook_event_name": "PostToolUse",
            "tool_name": "mcp__troupe_1__troupe_submit_result",
            "tool_input": {"value": {"status": "stored", "token": "ctx-token"}},
            "tool_response": [{"type": "text", "text": "result accepted"}],
        },
        {
            "hook_event_name": "PostToolUseFailure",
            "tool_name": "mcp__troupe_1__troupe_submit_result",
            "tool_input": {"value": {"token": "ctx-token", "confidence": 6}},
            "error": "result validation failed (invalid call 1/8)",
        },
        {
            "hook_event_name": "PostToolUse",
            "tool_name": "mcp__troupe_1__troupe_submit_result",
            "tool_input": {"value": {"token": "ctx-token", "confidence": 8}},
            "tool_response": [{"type": "text", "text": "result accepted"}],
        },
    ]

    del audit[missing_index]
    with pytest.raises(acceptance.AcceptanceFailure, match="schema correction"):
        acceptance._require_claude_schema_corrections(audit, seed_token="ctx-token")


def test_claude_live_acceptance_proves_context_recall_without_other_tools() -> None:
    acceptance = _live_acceptance_module()
    audit = [
        {
            "hook_event_name": "PreToolUse",
            "tool_name": "Read",
            "tool_input": {"file_path": "/tmp/seed.txt"},
        },
        {
            "hook_event_name": "PreToolUse",
            "tool_name": "Write",
            "tool_input": {"file_path": "/tmp/artifact.txt"},
        },
        {
            "hook_event_name": "PostToolUse",
            "tool_name": "mcp__troupe_1__troupe_submit_result",
            "tool_input": {"value": {"status": "stored", "token": "ctx-token"}},
            "tool_response": [{"type": "text", "text": "result accepted"}],
        },
        {
            "hook_event_name": "PreToolUse",
            "tool_name": "mcp__troupe_1__troupe_submit_result",
            "tool_input": {"value": {"token": "ctx-token", "confidence": 6}},
        },
        {
            "hook_event_name": "PreToolUse",
            "tool_name": "mcp__troupe_1__troupe_submit_result",
            "tool_input": {"value": {"token": "ctx-token", "confidence": 8}},
        },
        {
            "hook_event_name": "PostToolUse",
            "tool_name": "mcp__troupe_1__troupe_submit_result",
            "tool_input": {"value": {"token": "ctx-token", "confidence": 8}},
            "tool_response": [{"type": "text", "text": "result accepted"}],
        },
    ]

    acceptance._require_claude_context_recall(audit, seed_token="ctx-token")


def test_claude_live_acceptance_rejects_context_recall_with_file_access() -> None:
    acceptance = _live_acceptance_module()
    audit = [
        {
            "hook_event_name": "PreToolUse",
            "tool_name": "Read",
            "tool_input": {"file_path": "/tmp/seed.txt"},
        },
        {
            "hook_event_name": "PreToolUse",
            "tool_name": "Write",
            "tool_input": {"file_path": "/tmp/artifact.txt"},
        },
        {
            "hook_event_name": "PostToolUse",
            "tool_name": "mcp__troupe_1__troupe_submit_result",
            "tool_input": {"value": {"status": "stored", "token": "ctx-token"}},
            "tool_response": [{"type": "text", "text": "result accepted"}],
        },
        {
            "hook_event_name": "PreToolUse",
            "tool_name": "Read",
            "tool_input": {"file_path": "/tmp/schema-correction-audit.jsonl"},
        },
        {
            "hook_event_name": "PostToolUse",
            "tool_name": "mcp__troupe_1__troupe_submit_result",
            "tool_input": {"value": {"token": "ctx-token", "confidence": 8}},
            "tool_response": [{"type": "text", "text": "result accepted"}],
        },
    ]

    with pytest.raises(acceptance.AcceptanceFailure, match="context recall"):
        acceptance._require_claude_context_recall(audit, seed_token="ctx-token")
