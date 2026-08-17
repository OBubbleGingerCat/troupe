#!/usr/bin/env python3
"""Deterministic ACP process used by the diagnostics system E2E."""

from __future__ import annotations

import argparse
import http.client
import json
import os
import sys
from pathlib import Path
from typing import Any
from urllib.parse import urlsplit


RESULT_TOOL = "troupe_submit_result"


def _write(message: dict[str, object]) -> None:
    print(json.dumps(message, separators=(",", ":")), flush=True)


def _response(request_id: object, result: object) -> None:
    _write({"jsonrpc": "2.0", "id": request_id, "result": result})


def _notification(session_id: str, update: dict[str, object]) -> None:
    _write(
        {
            "jsonrpc": "2.0",
            "method": "session/update",
            "params": {"sessionId": session_id, "update": update},
        }
    )


def _record(path: Path, event: str, **fields: object) -> None:
    encoded = json.dumps(
        {"event": event, "pid": os.getpid(), **fields},
        sort_keys=True,
        separators=(",", ":"),
    ).encode()
    descriptor = os.open(path, os.O_WRONLY | os.O_CREAT | os.O_APPEND, 0o600)
    try:
        os.write(descriptor, encoded + b"\n")
    finally:
        os.close(descriptor)


def _select_option(
    identifier: str,
    current: str,
    values: tuple[str, ...],
    category: str,
) -> dict[str, object]:
    return {
        "id": identifier,
        "name": identifier.replace("_", " ").title(),
        "category": category,
        "type": "select",
        "currentValue": current,
        "options": [{"value": value, "name": value} for value in values],
    }


def _config_options(provider: str, values: dict[str, str]) -> list[dict[str, object]]:
    effort_id = {
        "codex": "reasoning_effort",
        "claude": "effort",
        "kimi": "thinking",
    }[provider]
    modes = ("default", "agent") if provider == "codex" else ("default", "plan")
    if provider == "kimi":
        modes = ("default", "plan", "auto", "yolo")
    return [
        _select_option("mode", values["mode"], modes, "mode"),
        _select_option(
            "model", values["model"], ("default-model", "test-model"), "model"
        ),
        _select_option(
            effort_id,
            values[effort_id],
            ("medium", "max"),
            "thought_level",
        ),
    ]


def _mcp_post(
    server: dict[str, Any],
    payload: dict[str, object],
    *,
    revision: str | None,
) -> tuple[int, bytes]:
    headers = {item["name"]: item["value"] for item in server["headers"]}
    parsed = urlsplit(server["url"])
    request_headers = {
        "Accept": "application/json, text/event-stream",
        "Authorization": headers["Authorization"],
        "Content-Type": "application/json",
    }
    if revision is not None:
        request_headers["MCP-Protocol-Version"] = revision
    connection = http.client.HTTPConnection(parsed.hostname, parsed.port, timeout=5)
    try:
        connection.request(
            "POST",
            parsed.path or "/",
            body=json.dumps(payload, separators=(",", ":")),
            headers=request_headers,
        )
        response = connection.getresponse()
        return response.status, response.read()
    finally:
        connection.close()


def _discover_mcp(server: dict[str, Any], revision: str) -> None:
    status, body = _mcp_post(
        server,
        {
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "protocolVersion": revision,
                "capabilities": {},
                "clientInfo": {"name": "troupe-diagnostics-e2e", "version": "1"},
            },
        },
        revision=None,
    )
    assert status == 200, (status, body)
    assert json.loads(body)["result"]["protocolVersion"] == revision
    status, body = _mcp_post(
        server,
        {
            "jsonrpc": "2.0",
            "method": "notifications/initialized",
            "params": {},
        },
        revision=revision,
    )
    assert status == 202 and body == b"", (status, body)
    status, body = _mcp_post(
        server,
        {"jsonrpc": "2.0", "id": 2, "method": "tools/list", "params": {}},
        revision=revision,
    )
    assert status == 200, (status, body)
    assert [tool["name"] for tool in json.loads(body)["result"]["tools"]] == [
        RESULT_TOOL
    ]


def _submit_result(
    server: dict[str, Any], revision: str, request_id: int, value: object
) -> dict[str, Any]:
    status, body = _mcp_post(
        server,
        {
            "jsonrpc": "2.0",
            "id": request_id,
            "method": "tools/call",
            "params": {"name": RESULT_TOOL, "arguments": {"value": value}},
        },
        revision=revision,
    )
    assert status == 200, (status, body)
    result = json.loads(body)
    assert isinstance(result, dict)
    return result


def _emit_turn_updates(session_id: str, turn: int) -> None:
    tool_id = f"mock-tool-{turn}"
    updates: tuple[dict[str, object], ...] = (
        {
            "sessionUpdate": "agent_thought_chunk",
            "content": {"type": "text", "text": f"thinking-{turn}"},
        },
        {
            "sessionUpdate": "agent_message_chunk",
            "content": {"type": "text", "text": f"message-{turn}-a"},
        },
        {
            "sessionUpdate": "agent_message_chunk",
            "content": {"type": "text", "text": "-b"},
        },
        {
            "sessionUpdate": "plan",
            "entries": [
                {
                    "content": f"complete turn {turn}",
                    "priority": "high",
                    "status": "in_progress",
                }
            ],
        },
        {"sessionUpdate": "usage_update", "used": turn * 100, "size": 4096},
        {
            "sessionUpdate": "tool_call",
            "toolCallId": tool_id,
            "title": f"Inspect turn {turn}",
            "kind": "read",
            "status": "in_progress",
            "rawInput": {
                "turn": turn,
                "credential": f"input-is-content-{turn}",
            },
        },
        {
            "sessionUpdate": "tool_call_update",
            "toolCallId": tool_id,
            "status": "completed",
            "rawOutput": {
                "turn": turn,
                "token": f"output-is-content-{turn}",
            },
            "content": [
                {
                    "type": "content",
                    "content": {"type": "text", "text": f"tool output {turn}"},
                }
            ],
            "locations": [{"path": f"turn-{turn}.txt", "line": turn}],
        },
    )
    for update in updates:
        _notification(session_id, update)


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--provider", choices=("codex", "claude", "kimi"), required=True)
    parser.add_argument("--mcp-revision", required=True)
    parser.add_argument("--events", type=Path, required=True)
    args = parser.parse_args()

    effort_id = {
        "codex": "reasoning_effort",
        "claude": "effort",
        "kimi": "thinking",
    }[args.provider]
    config = {"mode": "default", "model": "default-model", effort_id: "medium"}
    session_id: str | None = None
    server: dict[str, Any] | None = None
    turn = 0
    _record(args.events, "process_started", provider=args.provider)

    for raw_line in sys.stdin:
        request = json.loads(raw_line)
        method = request.get("method")
        request_id = request.get("id")
        params = request.get("params", {})
        _record(args.events, "acp_request", method=method)
        if method == "initialize":
            _response(
                request_id,
                {
                    "protocolVersion": 1,
                    "agentCapabilities": {
                        "loadSession": True,
                        "mcpCapabilities": {"http": True},
                    },
                    "authMethods": [],
                    "agentInfo": {
                        "name": "troupe-diagnostics-mock",
                        "title": "Troupe diagnostics mock",
                        "version": "0.31.1" if args.provider == "kimi" else "1",
                    },
                },
            )
        elif method == "session/new":
            servers = params["mcpServers"]
            assert len(servers) == 1 and servers[0]["type"] == "http"
            server = servers[0]
            _discover_mcp(server, args.mcp_revision)
            session_id = f"diagnostics-{args.provider}-{os.getpid()}"
            _record(args.events, "session_new", session_id=session_id)
            _response(
                request_id,
                {
                    "sessionId": session_id,
                    "configOptions": _config_options(args.provider, config),
                },
            )
        elif method == "session/set_config_option":
            config[params["configId"]] = params["value"]
            _response(
                request_id,
                {"configOptions": _config_options(args.provider, config)},
            )
        elif method == "session/prompt":
            assert server is not None and session_id is not None
            assert params["sessionId"] == session_id
            turn += 1
            _emit_turn_updates(session_id, turn)
            for rejection in range(1, turn + 1):
                rejected = _submit_result(
                    server,
                    args.mcp_revision,
                    turn * 100 + rejection,
                    {"value": 0},
                )
                assert rejected["result"]["isError"] is True, rejected
            value = {"value": turn}
            accepted = _submit_result(
                server, args.mcp_revision, turn * 100 + turn + 1, value
            )
            assert accepted["result"]["isError"] is False, accepted
            _record(args.events, "turn_completed", turn=turn, value=value)
            _response(
                request_id,
                {
                    "stopReason": "end_turn",
                    "usage": {
                        "totalTokens": turn * 30,
                        "inputTokens": turn * 20,
                        "outputTokens": turn * 10,
                        "thoughtTokens": turn,
                        "cachedReadTokens": turn * 2,
                        "cachedWriteTokens": turn * 3,
                    },
                },
            )
        else:
            raise AssertionError(f"unexpected ACP method: {method!r}")

    _record(args.events, "stdin_closed", turns=turn)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
