from __future__ import annotations

import argparse
import fcntl
import http.client
import json
import os
import socket
import subprocess
import sys
import threading
import time
from pathlib import Path
from typing import Any
from urllib.parse import urlsplit


MCP_REVISION = "2025-06-18"
RESULT_TOOL = "troupe_submit_result"
MCP_BODY_MAX_BYTES = 8 * 1024 * 1024
ACP_FRAME_MAX_BYTES = 16 * 1024 * 1024
_OUTPUT_LOCK = threading.Lock()


def _record(path: Path, event: str, **details: object) -> None:
    payload = json.dumps(
        {"event": event, "pid": os.getpid(), **details},
        sort_keys=True,
        separators=(",", ":"),
    )
    descriptor = os.open(path, os.O_WRONLY | os.O_CREAT | os.O_APPEND, 0o600)
    try:
        os.write(descriptor, payload.encode("utf-8") + b"\n")
    finally:
        os.close(descriptor)


def _response(
    request_id: object,
    result: object,
    *,
    frame_depth: int | None = None,
) -> None:
    if frame_depth is not None:
        assert isinstance(result, dict)
        nested: object = None
        for _ in range(frame_depth - 3):
            nested = {"nested": nested}
        result = {**result, "_meta": nested}
    with _OUTPUT_LOCK:
        print(
            json.dumps(
                {"jsonrpc": "2.0", "id": request_id, "result": result},
                separators=(",", ":"),
            ),
            flush=True,
        )


def _response_and_notification_batch(
    request_id: object,
    result: object,
    method: str,
    params: object,
) -> None:
    with _OUTPUT_LOCK:
        print(
            json.dumps(
                [
                    {"jsonrpc": "2.0", "id": request_id, "result": result},
                    {"jsonrpc": "2.0", "method": method, "params": params},
                ],
                separators=(",", ":"),
            ),
            flush=True,
        )


def _response_and_request_batch(
    request_id: object,
    result: object,
    reverse_request_id: object,
    method: str,
    params: object,
) -> None:
    with _OUTPUT_LOCK:
        print(
            json.dumps(
                [
                    {"jsonrpc": "2.0", "id": request_id, "result": result},
                    {
                        "jsonrpc": "2.0",
                        "id": reverse_request_id,
                        "method": method,
                        "params": params,
                    },
                ],
                separators=(",", ":"),
            ),
            flush=True,
        )


def _malformed_response_with_result_and_error(request_id: object) -> None:
    with _OUTPUT_LOCK:
        print(
            json.dumps(
                {
                    "jsonrpc": "2.0",
                    "id": request_id,
                    "result": {},
                    "error": {"code": -32603, "message": "ambiguous response"},
                },
                separators=(",", ":"),
            ),
            flush=True,
        )


def _scenario_depth(scenario: str, prefix: str) -> int | None:
    if not scenario.startswith(prefix):
        return None
    return int(scenario.removeprefix(prefix))


def _error(
    request_id: object,
    code: int,
    message: str,
    *,
    data: object | None = None,
) -> None:
    error = {"code": code, "message": message}
    if data is not None:
        error["data"] = data
    with _OUTPUT_LOCK:
        print(
            json.dumps(
                {
                    "jsonrpc": "2.0",
                    "id": request_id,
                    "error": error,
                },
                separators=(",", ":"),
            ),
            flush=True,
        )


def _request(request_id: object, method: str, params: object) -> None:
    with _OUTPUT_LOCK:
        print(
            json.dumps(
                {
                    "jsonrpc": "2.0",
                    "id": request_id,
                    "method": method,
                    "params": params,
                },
                separators=(",", ":"),
            ),
            flush=True,
        )


def _notification(method: str, params: object) -> None:
    with _OUTPUT_LOCK:
        print(
            json.dumps(
                {
                    "jsonrpc": "2.0",
                    "method": method,
                    "params": params,
                },
                separators=(",", ":"),
            ),
            flush=True,
        )


def _permission_params(session_id: str) -> dict[str, object]:
    return {
        "sessionId": session_id,
        "toolCall": {
            "toolCallId": "mock-tool-call",
            "kind": "execute",
            "title": "Run a command",
        },
        "options": [
            {
                "optionId": "allow-once",
                "name": "Allow once",
                "kind": "allow_once",
            },
            {
                "optionId": "reject-once",
                "name": "Reject once",
                "kind": "reject_once",
            },
        ],
    }


def _codex_option(
    option_id: str,
    kind: str,
    *,
    decision: str | None = None,
) -> dict[str, object]:
    option: dict[str, object] = {
        "optionId": option_id,
        "name": f"mock display text for {option_id}",
        "kind": kind,
    }
    if decision is not None:
        option["_meta"] = {"codex": {"decision": decision}}
    return option


def _codex_permission_cases(
    session_id: str,
) -> list[tuple[str, dict[str, object], dict[str, object]]]:
    def request(
        *,
        kind: str,
        options: list[dict[str, object]],
        meta: dict[str, object] | None = None,
    ) -> dict[str, object]:
        value: dict[str, object] = {
            "sessionId": session_id,
            "toolCall": {
                "toolCallId": "codex-permission-tool",
                "kind": kind,
                "status": "pending",
            },
            "options": options,
        }
        if meta is not None:
            value["_meta"] = meta
        return value

    def selected(option_id: str) -> dict[str, object]:
        return {"outcome": {"outcome": "selected", "optionId": option_id}}

    return [
        (
            "command",
            request(
                kind="execute",
                options=[
                    _codex_option("reject_once", "reject_once", decision="decline"),
                    _codex_option("allow_once", "allow_once", decision="accept"),
                    _codex_option(
                        "allow_always",
                        "allow_always",
                        decision="acceptForSession",
                    ),
                ],
                meta={"codex": {"params": {"itemId": "command-item"}}},
            ),
            selected("allow_once"),
        ),
        (
            "permissions",
            request(
                kind="other",
                options=[
                    _codex_option(
                        "allow_permissions_session",
                        "allow_always",
                        decision="allowPermissionsForSession",
                    ),
                    _codex_option(
                        "allow_permissions_turn",
                        "allow_once",
                        decision="allowPermissionsForTurn",
                    ),
                    _codex_option(
                        "reject_permissions",
                        "reject_once",
                        decision="rejectPermissions",
                    ),
                ],
                meta={"codex": {"params": {"itemId": "permissions-item"}}},
            ),
            selected("allow_permissions_turn"),
        ),
        (
            "mcp_tool",
            request(
                kind="execute",
                options=[
                    _codex_option("decline", "reject_once"),
                    _codex_option("allow_once", "allow_once"),
                    _codex_option("allow_session", "allow_always"),
                ],
                meta={"is_mcp_tool_approval": True},
            ),
            selected("allow_once"),
        ),
        (
            "plan_review",
            request(
                kind="switch_mode",
                options=[
                    _codex_option("revise_plan", "reject_once"),
                    _codex_option("implement_plan", "allow_once"),
                ],
                meta={
                    "codex": {
                        "kind": "plan_review",
                        "planItemId": "plan-item",
                    }
                },
            ),
            selected("implement_plan"),
        ),
        (
            "provider_question",
            request(
                kind="other",
                options=[
                    _codex_option("accept", "allow_once"),
                    _codex_option("decline", "reject_once"),
                ],
            ),
            selected("decline"),
        ),
        (
            "unknown",
            request(
                kind="execute",
                options=[
                    _codex_option("unknown-allow", "allow_once"),
                    _codex_option("unknown-reject", "reject_once"),
                ],
                meta={"future_adapter_shape": True},
            ),
            selected("unknown-reject"),
        ),
        (
            "ambiguous",
            request(
                kind="execute",
                options=[
                    _codex_option("reject-a", "reject_once"),
                    _codex_option("reject-b", "reject_once"),
                ],
                meta={"future_adapter_shape": True},
            ),
            {"outcome": {"outcome": "cancelled"}},
        ),
    ]


def _claude_permission_cases(
    session_id: str,
) -> list[tuple[str, dict[str, object], dict[str, object]]]:
    def option(option_id: str, kind: str) -> dict[str, object]:
        return {
            "optionId": option_id,
            "name": f"mock Claude display text for {option_id}",
            "kind": kind,
        }

    def request(
        *,
        kind: str,
        options: list[dict[str, object]],
        meta: dict[str, object] | None = None,
    ) -> dict[str, object]:
        value: dict[str, object] = {
            "sessionId": session_id,
            "toolCall": {
                "toolCallId": "claude-permission-tool",
                "kind": kind,
                "status": "pending",
            },
            "options": options,
        }
        if meta is not None:
            value["_meta"] = meta
        return value

    def selected(option_id: str) -> dict[str, object]:
        return {"outcome": {"outcome": "selected", "optionId": option_id}}

    return [
        (
            "tool",
            request(
                kind="execute",
                options=[
                    option("allow_always", "allow_always"),
                    option("reject", "reject_once"),
                    option("allow", "allow_once"),
                ],
            ),
            selected("allow"),
        ),
        (
            "exit_plan_mode",
            request(
                kind="switch_mode",
                options=[
                    option("acceptEdits", "allow_always"),
                    option("plan", "reject_once"),
                    option("default", "allow_once"),
                ],
            ),
            selected("default"),
        ),
        (
            "unknown",
            request(
                kind="execute",
                options=[
                    option("future-allow", "allow_once"),
                    option("future-reject", "reject_once"),
                ],
                meta={"future_adapter_shape": True},
            ),
            selected("future-reject"),
        ),
        (
            "ambiguous",
            request(
                kind="execute",
                options=[
                    option("reject-a", "reject_once"),
                    option("reject-b", "reject_once"),
                ],
                meta={"future_adapter_shape": True},
            ),
            {"outcome": {"outcome": "cancelled"}},
        ),
    ]


def _write_oversized_acp_frame() -> None:
    sys.stdout.buffer.write(b" " * (ACP_FRAME_MAX_BYTES + 1))
    sys.stdout.buffer.flush()


def _select_option(
    identifier: str,
    name: str,
    current: str,
    choices: tuple[str, ...],
    category: str,
) -> dict[str, object]:
    return {
        "id": identifier,
        "name": name,
        "category": category,
        "type": "select",
        "currentValue": current,
        "options": [{"value": choice, "name": choice} for choice in choices],
    }


def _config_options(
    values: dict[str, str],
    *,
    invalid_domain: str | None = None,
    include_mode: bool = True,
) -> list[dict[str, object]]:
    effort_id = next(
        (identifier for identifier in ("effort", "reasoning_effort") if identifier in values),
        None,
    )

    def choices(identifier: str, values_: tuple[str, ...]) -> tuple[str, ...]:
        if invalid_domain != identifier:
            return values_
        return tuple(value for value in values_ if value != values[identifier])

    mode_values = ("default", "plan") if "effort" in values else ("default", "agent")
    options = [
        _select_option(
            "mode",
            "Mode",
            values["mode"],
            choices("mode", mode_values),
            "mode",
        ),
        _select_option(
            "model",
            "Model",
            values["model"],
            choices("model", ("default-model", "test-model")),
            "model",
        ),
    ]
    if effort_id is not None:
        options.append(
            _select_option(
                effort_id,
                "Reasoning effort",
                values[effort_id],
                choices(effort_id, ("medium", "max")),
                "thought_level",
            )
        )
    if not include_mode:
        options = [option for option in options if option["id"] != "mode"]
    return options


def _mcp_post(
    connection: http.client.HTTPConnection,
    path: str,
    authorization: str,
    payload: dict[str, object],
    *,
    initialized: bool,
) -> tuple[int, bytes, int]:
    headers = {
        "Accept": "application/json, text/event-stream",
        "Authorization": authorization,
        "Content-Type": "application/json",
    }
    if initialized:
        headers["MCP-Protocol-Version"] = MCP_REVISION
    connection.request(
        "POST",
        path,
        body=json.dumps(payload, separators=(",", ":")),
        headers=headers,
    )
    assert connection.sock is not None
    client_port = connection.sock.getsockname()[1]
    response = connection.getresponse()
    body = response.read()
    status = response.status
    return status, body, client_port


def _mcp_http_10_initialize(server: dict[str, Any]) -> int:
    headers = {item["name"]: item["value"] for item in server["headers"]}
    parsed = urlsplit(server["url"])
    payload = json.dumps(
        {
            "jsonrpc": "2.0",
            "id": 10,
            "method": "initialize",
            "params": {
                "protocolVersion": MCP_REVISION,
                "capabilities": {},
                "clientInfo": {"name": "troupe-test-agent", "version": "1"},
            },
        },
        separators=(",", ":"),
    ).encode("utf-8")
    request = (
        f"POST {parsed.path or '/'} HTTP/1.0\r\n"
        f"Host: {parsed.hostname}:{parsed.port}\r\n"
        "Accept: application/json, text/event-stream\r\n"
        f"Authorization: {headers['Authorization']}\r\n"
        "Content-Type: application/json\r\n"
        f"Content-Length: {len(payload)}\r\n"
        "\r\n"
    ).encode("ascii") + payload
    with socket.create_connection((parsed.hostname, parsed.port)) as connection:
        connection.sendall(request)
        response = b""
        while True:
            chunk = connection.recv(4096)
            if not chunk:
                break
            response += chunk
    status_line = response.split(b"\r\n", 1)[0]
    return int(status_line.split(b" ", 2)[1])


def _discover_mcp(
    server: dict[str, Any],
    events: Path,
    invocation: int,
    *,
    require_single_connection: bool = False,
) -> None:
    headers = {item["name"]: item["value"] for item in server["headers"]}
    authorization = headers["Authorization"]
    parsed = urlsplit(server["url"])
    connection = http.client.HTTPConnection(parsed.hostname, parsed.port)
    try:
        status, body, connection_port = _mcp_post(
            connection,
            parsed.path or "/",
            authorization,
            {
                "jsonrpc": "2.0",
                "id": 1,
                "method": "initialize",
                "params": {
                    "protocolVersion": MCP_REVISION,
                    "capabilities": {},
                    "clientInfo": {"name": "troupe-test-agent", "version": "1"},
                },
            },
            initialized=False,
        )
        assert status == 200, (status, body)
        initialize = json.loads(body)
        assert initialize["result"]["protocolVersion"] == MCP_REVISION
        _record(
            events,
            "mcp_initialize",
            invocation=invocation,
            connection_port=connection_port,
        )

        status, body, initialized_port = _mcp_post(
            connection,
            parsed.path or "/",
            authorization,
            {
                "jsonrpc": "2.0",
                "method": "notifications/initialized",
                "params": {},
            },
            initialized=True,
        )
        if require_single_connection:
            assert initialized_port == connection_port
        assert status == 202 and body == b"", (status, body)
        _record(
            events,
            "mcp_initialized",
            invocation=invocation,
            connection_port=initialized_port,
        )

        status, body, tools_port = _mcp_post(
            connection,
            parsed.path or "/",
            authorization,
            {"jsonrpc": "2.0", "id": 2, "method": "tools/list", "params": {}},
            initialized=True,
        )
        if require_single_connection:
            assert tools_port == connection_port
        assert status == 200, (status, body)
        tools = json.loads(body)["result"]["tools"]
        assert [tool["name"] for tool in tools] == [RESULT_TOOL]
        _record(
            events,
            "mcp_tools_list",
            invocation=invocation,
            connection_port=tools_port,
        )
        if require_single_connection:
            status, body, reused_port = _mcp_post(
                connection,
                parsed.path or "/",
                authorization,
                {"jsonrpc": "2.0", "id": 3, "method": "tools/list", "params": {}},
                initialized=True,
            )
            assert reused_port == connection_port
            assert status == 200
            assert json.loads(body)["error"]["code"] == -32600
            _record(
                events,
                "mcp_post_ready_connection_reused",
                invocation=invocation,
                connection_port=reused_port,
            )
    finally:
        connection.close()


def _submit_result(
    server: dict[str, Any],
    request_id: int,
    value: object,
) -> dict[str, Any]:
    headers = {item["name"]: item["value"] for item in server["headers"]}
    parsed = urlsplit(server["url"])
    connection = http.client.HTTPConnection(parsed.hostname, parsed.port)
    try:
        status, body, _ = _mcp_post(
            connection,
            parsed.path or "/",
            headers["Authorization"],
            {
                "jsonrpc": "2.0",
                "id": request_id,
                "method": "tools/call",
                "params": {
                    "name": RESULT_TOOL,
                    "arguments": {"value": value},
                },
            },
            initialized=True,
        )
    finally:
        connection.close()
    assert status == 200, (status, body)
    return json.loads(body)


def _submit_raw_tool_call(
    server: dict[str, Any],
    request_id: int,
    *,
    name: str = RESULT_TOOL,
    arguments: object,
    authorization: str | None = None,
) -> tuple[int, dict[str, Any] | None]:
    headers = {item["name"]: item["value"] for item in server["headers"]}
    parsed = urlsplit(server["url"])
    connection = http.client.HTTPConnection(parsed.hostname, parsed.port)
    try:
        status, body, _ = _mcp_post(
            connection,
            parsed.path or "/",
            authorization or headers["Authorization"],
            {
                "jsonrpc": "2.0",
                "id": request_id,
                "method": "tools/call",
                "params": {"name": name, "arguments": arguments},
            },
            initialized=True,
        )
    finally:
        connection.close()
    return status, json.loads(body) if body else None


def _post_tool_body(
    server: dict[str, Any],
    body: bytes | list[bytes],
    *,
    chunked: bool = False,
) -> tuple[int, bytes, str | None]:
    headers = {item["name"]: item["value"] for item in server["headers"]}
    parsed = urlsplit(server["url"])
    request_headers = {
        "Accept": "application/json, text/event-stream",
        "Authorization": headers["Authorization"],
        "Content-Type": "application/json",
        "MCP-Protocol-Version": MCP_REVISION,
    }
    connection = http.client.HTTPConnection(parsed.hostname, parsed.port)
    try:
        connection.request(
            "POST",
            parsed.path or "/",
            body=body,
            headers=request_headers,
            encode_chunked=chunked,
        )
        response = connection.getresponse()
        return response.status, response.read(), response.getheader("Connection")
    finally:
        connection.close()


def _post_declared_oversized_body(server: dict[str, Any], size: int) -> bytes:
    headers = {item["name"]: item["value"] for item in server["headers"]}
    parsed = urlsplit(server["url"])
    request = (
        f"POST {parsed.path or '/'} HTTP/1.1\r\n"
        f"Host: {parsed.hostname}:{parsed.port}\r\n"
        "Accept: application/json, text/event-stream\r\n"
        f"Authorization: {headers['Authorization']}\r\n"
        "Content-Type: application/json\r\n"
        f"MCP-Protocol-Version: {MCP_REVISION}\r\n"
        f"Content-Length: {size}\r\n"
        "\r\n"
    ).encode("ascii")
    with socket.create_connection((parsed.hostname, parsed.port)) as connection:
        connection.sendall(request)
        response = b""
        while True:
            chunk = connection.recv(4096)
            if not chunk:
                break
            response += chunk
    return response


def _open_partial_route_request(server: dict[str, Any]) -> socket.socket:
    headers = {item["name"]: item["value"] for item in server["headers"]}
    parsed = urlsplit(server["url"])
    connection = socket.create_connection((parsed.hostname, parsed.port))
    request_headers = (
        f"POST {parsed.path or '/'} HTTP/1.1\r\n"
        f"Host: {parsed.hostname}:{parsed.port}\r\n"
        "Accept: application/json, text/event-stream\r\n"
        f"Authorization: {headers['Authorization']}\r\n"
        "Content-Type: application/json\r\n"
        f"MCP-Protocol-Version: {MCP_REVISION}\r\n"
        "Content-Length: 4096\r\n"
        "Expect: 100-continue\r\n"
        "\r\n"
    )
    connection.sendall(request_headers.encode("ascii"))
    response = b""
    while b"\r\n\r\n" not in response:
        response += connection.recv(4096)
    assert response.startswith(b"HTTP/1.1 100 Continue\r\n"), response
    connection.sendall(b"{")
    return connection


def _require_closed_connection(connection: socket.socket) -> None:
    connection.settimeout(1.0)
    try:
        received = connection.recv(1)
    except ConnectionResetError:
        return
    assert received == b"", received


def _claim_attempt(path: Path | None) -> int:
    if path is None:
        return 1
    descriptor = os.open(path, os.O_RDWR | os.O_CREAT, 0o600)
    try:
        fcntl.flock(descriptor, fcntl.LOCK_EX)
        raw = os.read(descriptor, 64)
        attempt = int(raw) + 1 if raw else 1
        os.lseek(descriptor, 0, os.SEEK_SET)
        os.ftruncate(descriptor, 0)
        os.write(descriptor, str(attempt).encode("ascii"))
        return attempt
    finally:
        os.close(descriptor)


def main() -> int:
    global MCP_REVISION

    parser = argparse.ArgumentParser()
    parser.add_argument("--events", type=Path, required=True)
    parser.add_argument("--scenario", default="ready")
    parser.add_argument("--release", type=Path)
    parser.add_argument("--results-json")
    parser.add_argument("--attempt-file", type=Path)
    parser.add_argument("--mcp-revision", default=MCP_REVISION)
    args = parser.parse_args()
    MCP_REVISION = args.mcp_revision

    attempt = _claim_attempt(args.attempt_file)

    cwd_metadata = os.stat(".")
    _record(
        args.events,
        "process_started",
        cwd=os.getcwd(),
        cwd_dev=cwd_metadata.st_dev,
        cwd_ino=cwd_metadata.st_ino,
        attempt=attempt,
    )
    if args.scenario == "codex_path_scrubbed":
        _record(
            args.events,
            "codex_path_observed",
            value=os.environ.get("CODEX_PATH"),
        )
    if args.scenario in {
        "spawn_descendant",
        "spawn_detached_descendant",
        "spawn_detached_descendant_then_exit",
    }:
        descendant = subprocess.Popen(
            [sys.executable, "-c", "import signal; signal.pause()"],
            stdin=subprocess.DEVNULL,
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
            start_new_session=args.scenario != "spawn_descendant",
        )
        _record(args.events, "descendant_started", descendant_pid=descendant.pid)
    config = {
        "mode": "default",
        "model": "default-model",
        (
            "effort"
            if args.scenario.startswith("claude_")
            else "reasoning_effort"
        ): "medium",
    }
    if args.scenario == "claude_effort_option_absent":
        del config["effort"]
    session_invocations = 0
    partial_route_connection: socket.socket | None = None
    pending_mcp_server: dict[str, Any] | None = None
    pending_mcp_invocation = 0
    current_mcp_server: dict[str, Any] | None = None
    current_session_id: str | None = None
    prompt_turn = 0
    remembered_token: str | None = None
    pending_prompt_id: object | None = None
    pending_permission_id: object | None = None
    pending_permission_expected: dict[str, object] | None = None
    pending_permission_case: str | None = None
    codex_permission_queue: list[
        tuple[str, dict[str, object], dict[str, object]]
    ] = []
    claude_permission_queue: list[
        tuple[str, dict[str, object], dict[str, object]]
    ] = []
    background_threads: list[threading.Thread] = []

    def send_claude_mode_transition(mode: str) -> None:
        assert current_session_id is not None
        _notification(
            "session/update",
            {
                "sessionId": current_session_id,
                "update": {
                    "sessionUpdate": "current_mode_update",
                    "currentModeId": mode,
                },
            },
        )
        _record(args.events, "claude_mode_update_sent", mode=mode)
        config["mode"] = mode
        _notification(
            "session/update",
            {
                "sessionId": current_session_id,
                "update": {
                    "sessionUpdate": "config_option_update",
                    "configOptions": _config_options(config),
                },
            },
        )
        _record(args.events, "claude_config_mode_snapshot_sent", mode=mode)

    for raw_line in sys.stdin:
        request = json.loads(raw_line)
        method = request.get("method")
        request_id = request.get("id")
        params = request.get("params", {})
        _record(args.events, "acp_request", method=method)

        if method is None and request_id == pending_permission_id:
            if args.scenario == "codex_permission_matrix":
                assert request.get("result") == pending_permission_expected, request
                assert pending_permission_case is not None
                _record(
                    args.events,
                    "codex_permission_response_received",
                    permission_case=pending_permission_case,
                    result=request.get("result"),
                )
                if codex_permission_queue:
                    (
                        pending_permission_case,
                        permission_params,
                        pending_permission_expected,
                    ) = codex_permission_queue.pop(0)
                    pending_permission_id = (
                        f"codex-permission-{pending_permission_case}"
                    )
                    _request(
                        pending_permission_id,
                        "session/request_permission",
                        permission_params,
                    )
                    continue
                assert current_mcp_server is not None
                assert pending_prompt_id is not None
                accepted = _submit_result(
                    current_mcp_server,
                    990,
                    {"value": 7},
                )
                assert accepted["result"]["isError"] is False, accepted
                _record(args.events, "result_submitted", turn=prompt_turn, value={"value": 7})
                _response(pending_prompt_id, {"stopReason": "end_turn"})
                pending_permission_id = None
                pending_permission_expected = None
                pending_permission_case = None
                pending_prompt_id = None
                continue
            if args.scenario == "claude_permission_matrix":
                assert request.get("result") == pending_permission_expected, request
                assert pending_permission_case is not None
                completed_permission_case = pending_permission_case
                _record(
                    args.events,
                    "claude_permission_response_received",
                    permission_case=pending_permission_case,
                    result=request.get("result"),
                )
                if completed_permission_case == "exit_plan_mode":
                    send_claude_mode_transition("default")
                if claude_permission_queue:
                    (
                        pending_permission_case,
                        permission_params,
                        pending_permission_expected,
                    ) = claude_permission_queue.pop(0)
                    pending_permission_id = (
                        f"claude-permission-{pending_permission_case}"
                    )
                    if pending_permission_case == "exit_plan_mode":
                        send_claude_mode_transition("plan")
                    _request(
                        pending_permission_id,
                        "session/request_permission",
                        permission_params,
                    )
                    continue
                assert current_mcp_server is not None
                assert pending_prompt_id is not None
                accepted = _submit_result(
                    current_mcp_server,
                    991,
                    {"value": 8},
                )
                assert accepted["result"]["isError"] is False, accepted
                _record(
                    args.events,
                    "result_submitted",
                    turn=prompt_turn,
                    value={"value": 8},
                )
                _response(pending_prompt_id, {"stopReason": "end_turn"})
                pending_permission_id = None
                pending_permission_expected = None
                pending_permission_case = None
                pending_prompt_id = None
                continue
            if args.scenario in {
                "claude_plan_mode_not_restored",
                "claude_plan_mode_restored_after_response",
            }:
                assert request.get("result") == pending_permission_expected, request
                assert current_mcp_server is not None
                assert pending_prompt_id is not None
                _record(
                    args.events,
                    "claude_unrestored_permission_response_received",
                    scenario=args.scenario,
                )
                if args.scenario == "claude_plan_mode_restored_after_response":
                    accepted = _submit_result(
                        current_mcp_server,
                        993,
                        {"value": 10},
                    )
                    assert accepted["result"]["isError"] is False, accepted
                    _record(
                        args.events,
                        "result_submitted",
                        turn=prompt_turn,
                        value={"value": 10},
                    )
                    _response(pending_prompt_id, {"stopReason": "end_turn"})
                    _record(args.events, "claude_prompt_response_sent_before_restore")
                assert args.release is not None
                with args.release.open("rb", buffering=0) as release:
                    assert release.read(1) == b"1"
                if args.scenario == "claude_plan_mode_restored_after_response":
                    send_claude_mode_transition("default")
                    pending_permission_id = None
                    pending_permission_expected = None
                    pending_permission_case = None
                    pending_prompt_id = None
                    continue
                accepted = _submit_result(
                    current_mcp_server,
                    992,
                    {"value": 9},
                )
                assert accepted["result"]["isError"] is False, accepted
                _record(
                    args.events,
                    "result_submitted",
                    turn=prompt_turn,
                    value={"value": 9},
                )
                _response(pending_prompt_id, {"stopReason": "end_turn"})
                pending_permission_id = None
                pending_permission_expected = None
                pending_permission_case = None
                pending_prompt_id = None
                continue
            if args.scenario in {
                "act_late_permission_after_terminal",
                "act_terminal_then_permission_batch",
            }:
                _record(
                    args.events,
                    "late_permission_response_received",
                    scenario=args.scenario,
                    result=request.get("result"),
                )
                pending_permission_id = None
                continue
            assert request.get("result") == {"outcome": {"outcome": "cancelled"}}, request
            assert pending_prompt_id is not None
            assert args.release is not None
            _record(args.events, "permission_cancelled_response_received")

            def settle_after_permission(request_id: object = pending_prompt_id) -> None:
                assert args.release is not None
                with args.release.open("rb", buffering=0) as release:
                    assert release.read(1) == b"1"
                _response(request_id, {"stopReason": "cancelled"})
                _record(args.events, "cancel_settled", turn=1)

            settlement = threading.Thread(target=settle_after_permission)
            settlement.start()
            background_threads.append(settlement)
            pending_permission_id = None
            continue

        if method == "initialize":
            opening_crash = (
                args.scenario == "opening_eof_crash_loop"
                or (
                    args.scenario == "opening_crash_twice_then_ready"
                    and attempt <= 2
                )
            )
            if opening_crash:
                _record(args.events, "opening_transport_closing", attempt=attempt)
                with _OUTPUT_LOCK:
                    os.close(sys.stdout.fileno())
                continue
            if args.scenario == "oversized_acp_frame":
                _write_oversized_acp_frame()
                continue
            if args.scenario == "malformed_initialize_response_envelope":
                _record(args.events, "malformed_initialize_response_sent")
                _malformed_response_with_result_and_error(request_id)
                continue
            if args.scenario == "unknown_initialize_response_id":
                _record(args.events, "unknown_initialize_response_id_sent")
                request_id = "not-the-request-id"
            if args.scenario == "hold_initialize":
                assert args.release is not None
                _record(args.events, "initialize_blocked")
                while not args.release.exists():
                    time.sleep(0.01)
            _response(
                request_id,
                {
                    "protocolVersion": 1,
                    "agentCapabilities": {
                        "loadSession": True,
                        "mcpCapabilities": {
                            "http": args.scenario != "no_http_mcp"
                        }
                    },
                    "authMethods": [
                        {"id": "mock-credential", "name": "Mock credential"}
                    ],
                    "agentInfo": {
                        "name": "troupe-mock",
                        "title": "Troupe Mock Agent",
                        "version": "1",
                    },
                },
                frame_depth=_scenario_depth(args.scenario, "opening_acp_depth_"),
            )
        elif method == "session/new":
            session_invocations += 1
            servers = params["mcpServers"]
            assert len(servers) == 1 and servers[0]["type"] == "http"
            server = servers[0]
            current_mcp_server = server
            acp_cwd_metadata = os.stat(params["cwd"])
            _record(
                args.events,
                "session_new_received",
                invocation=session_invocations,
                server_name=server["name"],
                url=server["url"],
                cwd=params["cwd"],
                cwd_dev=acp_cwd_metadata.st_dev,
                cwd_ino=acp_cwd_metadata.st_ino,
            )
            if args.scenario == "oversized_acp_frame_session_new":
                _record(args.events, "oversized_acp_frame_sent", phase="session_new")
                _write_oversized_acp_frame()
                continue
            if (
                args.scenario == "opening_transient_four_times_then_ready"
                and attempt <= 4
            ):
                _record(
                    args.events,
                    "opening_transient_error_sent",
                    attempt=attempt,
                    phase="session_new",
                )
                _error(request_id, -32099, "mock typed transient startup failure")
                continue
            if args.scenario == "http_10_before_discovery":
                status = _mcp_http_10_initialize(server)
                _record(args.events, "mcp_http_10_rejected", status=status)
                assert status == 505
            delayed_mcp = args.scenario in {
                "mcp_before_configuration",
                "mcp_between_configuration",
                "mcp_after_configuration",
                "opening_eof_before_mcp_ready",
                "opening_protocol_error_before_mcp_ready",
            }
            if delayed_mcp:
                pending_mcp_server = server
                pending_mcp_invocation = session_invocations
            else:
                _discover_mcp(
                    server,
                    args.events,
                    session_invocations,
                    require_single_connection=(
                        args.scenario == "single_connection_discovery"
                    ),
                )

            if (
                args.scenario == "auth_once_partial_connection"
                and session_invocations == 1
            ):
                partial_route_connection = _open_partial_route_request(server)
                _record(args.events, "old_route_partial_connection_open")

            if args.scenario == "session_new_error":
                _error(request_id, -32603, "session creation failed")
                continue
            if args.scenario in {"auth_once", "auth_once_partial_connection"}:
                _record(args.events, "auth_required_sent")
                _error(request_id, -32000, "Authentication required")
                if partial_route_connection is not None:
                    _require_closed_connection(partial_route_connection)
                    partial_route_connection.close()
                    partial_route_connection = None
                    _record(
                        args.events,
                        "old_route_connection_closed_after_auth_required",
                    )
                continue
            if args.scenario == "hold_before_ready":
                assert args.release is not None
                _record(args.events, "ready_blocked")
                while not args.release.exists():
                    time.sleep(0.01)
            session_id = f"mock-session-{os.getpid()}-{session_invocations}"
            current_session_id = session_id
            _record(
                args.events,
                "session_new_responding",
                invocation=session_invocations,
                session_id=session_id,
            )
            _response(
                request_id,
                {
                    "sessionId": session_id,
                    **(
                        {
                            "modes": {
                                "currentModeId": "default",
                                "availableModes": [
                                    {"id": "default", "name": "Default"},
                                    {"id": "agent", "name": "Agent"},
                                ],
                            }
                        }
                        if args.scenario
                        in {
                            "legacy_mode",
                            "legacy_mode_drift_before_model",
                            "legacy_mode_missing",
                        }
                        else {}
                    ),
                    "configOptions": (
                        []
                        if args.scenario == "configuration_invalid"
                        else _config_options(
                            config,
                            include_mode=args.scenario
                            not in {
                                "legacy_mode",
                                "legacy_mode_drift_before_model",
                                "legacy_mode_missing",
                            },
                        )
                    ),
                },
            )
            if args.scenario == "opening_session_scoped_update":
                _notification(
                    "session/update",
                    {
                        "sessionId": session_id,
                        "update": {
                            "sessionUpdate": "available_commands_update",
                            "availableCommands": [
                                {
                                    "name": "codex-command",
                                    "description": "mock Codex command",
                                }
                            ],
                        },
                    },
                )
                _record(args.events, "opening_session_scoped_update_sent")
            if args.scenario in {
                "exit_before_turn_intake",
                "idle_clean_eof",
                "spawn_detached_descendant_then_exit",
            }:
                assert args.release is not None

                def terminate_after_release() -> None:
                    assert args.release is not None
                    with args.release.open("rb", buffering=0) as release:
                        assert release.read(1) == b"1"
                    if args.scenario in {
                        "exit_before_turn_intake",
                        "spawn_detached_descendant_then_exit",
                    }:
                        _record(args.events, "process_exiting_before_turn_intake")
                        os._exit(0)
                    _record(args.events, "idle_stdout_closing")
                    with _OUTPUT_LOCK:
                        os.close(sys.stdout.fileno())
                    _record(args.events, "idle_stdout_closed")

                terminal = threading.Thread(target=terminate_after_release)
                terminal.start()
                background_threads.append(terminal)
            if args.scenario == "mcp_before_configuration":
                assert pending_mcp_server is not None
                _discover_mcp(
                    pending_mcp_server,
                    args.events,
                    pending_mcp_invocation,
                )
                pending_mcp_server = None
        elif method == "session/prompt" and (
            args.scenario in {
                "act_accepted_non_end_turn",
                "act_body_limits",
                "act_custom_correction",
                "act_callback_fault_then_request_error",
                "act_cancel_after_result_then_reuse",
                "act_cancel_during_callback_then_reuse",
                "act_cancel_permission_then_reuse",
                "act_cancel_then_reuse",
                "act_cancel_transport_lost",
                "act_digit_limit_integer",
                "act_invalid_then_request_error",
                "act_invalid_then_transport_loss",
                "act_malformed_prompt_response",
                "act_malformed_prompt_response_envelope",
                "act_ninth_invalid_then_request_error",
                "act_no_result",
                "act_late_permission_after_terminal",
                "act_late_update_after_terminal",
                "act_ordinary_update_burst",
                "act_oversized_acp_frame",
                "act_post_ready_auth_required",
                "act_post_ready_uncertain_error",
                "act_request_error_then_success",
                "act_request_error_internal_collision_then_success",
                "act_request_error_parse_collision_then_success",
                "act_request_error_transport_collision_then_success",
                "codex_authentication_lost",
                "act_result_matrix",
                "act_result_rejected_then_reuse",
                "act_settle_during_callback",
                "act_submit_results",
                "act_submit_concurrent",
                "act_two_turns",
                "codex_permission_matrix",
                "codex_uncertain_prompt_error",
                "codex_usage_limit_then_success",
                "claude_authentication_lost",
                "claude_cancel_end_turn_race",
                "claude_permission_matrix",
                "claude_plan_mode_not_restored",
                "claude_plan_mode_restored_after_response",
                "claude_rate_limit_then_success",
                "claude_synthetic_cancel",
                "claude_synthetic_cancel_detached_descendant",
                "claude_uncertain_prompt_error",
                "act_terminal_then_permission_batch",
                "act_unknown_prompt_response_id",
                "opening_crash_twice_then_ready",
                "post_ready_config_after_result",
                "post_ready_config_during_turn",
            }
            or args.scenario.startswith("act_acp_depth_")
        ):
            assert current_mcp_server is not None
            assert params["sessionId"] == current_session_id
            prompt_turn += 1
            prompt = params["prompt"]
            assert len(prompt) == 1 and prompt[0]["type"] == "text"
            prompt_text = prompt[0]["text"]
            _record(
                args.events,
                "prompt_received",
                turn=prompt_turn,
                session_id=params["sessionId"],
                prompt=prompt_text,
            )
            if args.scenario == "codex_permission_matrix":
                assert current_session_id is not None
                pending_prompt_id = request_id
                codex_permission_queue = _codex_permission_cases(current_session_id)
                (
                    pending_permission_case,
                    permission_params,
                    pending_permission_expected,
                ) = codex_permission_queue.pop(0)
                pending_permission_id = f"codex-permission-{pending_permission_case}"
                _request(
                    pending_permission_id,
                    "session/request_permission",
                    permission_params,
                )
                continue
            if args.scenario == "claude_permission_matrix":
                assert current_session_id is not None
                pending_prompt_id = request_id
                claude_permission_queue = _claude_permission_cases(current_session_id)
                (
                    pending_permission_case,
                    permission_params,
                    pending_permission_expected,
                ) = claude_permission_queue.pop(0)
                pending_permission_id = (
                    f"claude-permission-{pending_permission_case}"
                )
                _request(
                    pending_permission_id,
                    "session/request_permission",
                    permission_params,
                )
                continue
            if args.scenario in {
                "claude_plan_mode_not_restored",
                "claude_plan_mode_restored_after_response",
            }:
                assert current_session_id is not None
                pending_prompt_id = request_id
                send_claude_mode_transition("plan")
                (
                    pending_permission_case,
                    permission_params,
                    pending_permission_expected,
                ) = _claude_permission_cases(current_session_id)[1]
                pending_permission_id = "claude-permission-exit-plan-mode-unrestored"
                _request(
                    pending_permission_id,
                    "session/request_permission",
                    permission_params,
                )
                continue
            cancellation_scenarios = {
                "act_cancel_after_result_then_reuse",
                "act_cancel_during_callback_then_reuse",
                "act_cancel_permission_then_reuse",
                "act_cancel_then_reuse",
                "act_cancel_transport_lost",
                "act_result_rejected_then_reuse",
                "claude_cancel_end_turn_race",
                "claude_synthetic_cancel",
                "claude_synthetic_cancel_detached_descendant",
            }
            if args.scenario in cancellation_scenarios:
                if prompt_turn == 1:
                    pending_prompt_id = request_id
                    if (
                        args.scenario
                        == "claude_synthetic_cancel_detached_descendant"
                    ):
                        descendant = subprocess.Popen(
                            [
                                sys.executable,
                                "-c",
                                "import signal; signal.pause()",
                            ],
                            stdin=subprocess.DEVNULL,
                            stdout=subprocess.DEVNULL,
                            stderr=subprocess.DEVNULL,
                            start_new_session=True,
                        )
                        _record(
                            args.events,
                            "descendant_started",
                            descendant_pid=descendant.pid,
                        )
                    if args.scenario == "act_cancel_after_result_then_reuse":
                        accepted = _submit_result(
                            current_mcp_server,
                            900,
                            {"value": 1},
                        )
                        assert accepted["result"]["isError"] is False, accepted
                        _record(args.events, "result_submitted", turn=1, value={"value": 1})
                    elif args.scenario == "act_cancel_during_callback_then_reuse":
                        def submit_callback_value() -> None:
                            response = _submit_result(
                                current_mcp_server,
                                901,
                                {"value": 1},
                            )
                            _record(
                                args.events,
                                "callback_tool_result_finished",
                                is_error=response["result"]["isError"],
                                text=response["result"]["content"][0]["text"],
                            )

                        submission = threading.Thread(target=submit_callback_value)
                        submission.start()
                        background_threads.append(submission)
                    elif args.scenario == "act_result_rejected_then_reuse":
                        def submit_invalid_values() -> None:
                            for index in range(9):
                                response = _submit_result(
                                    current_mcp_server,
                                    910 + index,
                                    {"value": "invalid"},
                                )
                                assert response["result"]["isError"] is True, response
                                _record(
                                    args.events,
                                    "invalid_result_submitted",
                                    invalid_call=index + 1,
                                    text=response["result"]["content"][0]["text"],
                                )

                        submission = threading.Thread(target=submit_invalid_values)
                        submission.start()
                        background_threads.append(submission)
                    continue

                value = {"value": prompt_turn}
                accepted = _submit_result(
                    current_mcp_server,
                    950 + prompt_turn,
                    value,
                )
                assert accepted["result"]["isError"] is False, accepted
                _record(args.events, "result_submitted", turn=prompt_turn, value=value)
                _response(request_id, {"stopReason": "end_turn"})
                continue
            if args.scenario == "act_oversized_acp_frame":
                _record(args.events, "oversized_acp_frame_sent")
                _write_oversized_acp_frame()
                continue
            if args.scenario == "post_ready_config_during_turn":
                continue
            acp_depth = _scenario_depth(args.scenario, "act_acp_depth_")
            if acp_depth is not None:
                _record(args.events, "acp_depth_frame_sent", depth=acp_depth)
                _response(
                    request_id,
                    {"stopReason": "end_turn"},
                    frame_depth=acp_depth,
                )
                continue
            if args.scenario == "act_malformed_prompt_response":
                _record(args.events, "malformed_prompt_response_sent")
                _response(request_id, {"stopReason": "unknown_stop_reason"})
                continue
            if args.scenario == "act_malformed_prompt_response_envelope":
                _record(args.events, "malformed_prompt_response_envelope_sent")
                _malformed_response_with_result_and_error(request_id)
                continue
            if args.scenario == "act_unknown_prompt_response_id":
                _record(args.events, "unknown_prompt_response_id_sent")
                _response("not-the-prompt-id", {"stopReason": "end_turn"})
                continue
            if args.scenario == "act_no_result":
                _record(args.events, "turn_ended_without_result", turn=prompt_turn)
                _response(request_id, {"stopReason": "end_turn"})
                continue
            if args.scenario == "act_ordinary_update_burst":
                for index in range(256):
                    _notification(
                        "session/update",
                        {
                            "sessionId": current_session_id,
                            "update": {
                                "sessionUpdate": "agent_message_chunk",
                                "content": {
                                    "type": "text",
                                    "text": f"ordinary diagnostic update {index}",
                                },
                            },
                        },
                    )
                _record(args.events, "ordinary_updates_sent", count=256)
            if args.scenario == "act_invalid_then_transport_loss":
                rejected = _submit_result(
                    current_mcp_server,
                    699,
                    {"value": "not-an-integer"},
                )
                assert rejected["result"]["isError"] is True, rejected
                _record(
                    args.events,
                    "tool_result_received",
                    index=0,
                    is_error=True,
                    text=rejected["result"]["content"][0]["text"],
                )
                _record(args.events, "transport_closing_after_invalid")
                return 0
            if args.scenario in {
                "act_post_ready_auth_required",
                "act_post_ready_uncertain_error",
            }:
                if args.scenario == "act_post_ready_uncertain_error":
                    accepted = _submit_result(
                        current_mcp_server,
                        880,
                        {"value": 9},
                    )
                    assert accepted["result"]["isError"] is False, accepted
                    _record(args.events, "result_submitted_before_uncertain_error")
                code = (
                    -32000
                    if args.scenario == "act_post_ready_auth_required"
                    else -32800
                )
                _record(args.events, "prompt_terminal_error_sent", code=code)
                _error(request_id, code, "mock terminal prompt error")
                continue
            if args.scenario in {
                "codex_authentication_lost",
                "codex_uncertain_prompt_error",
                "codex_usage_limit_then_success",
            } and (args.scenario != "codex_usage_limit_then_success" or prompt_turn == 1):
                data: object | None
                if args.scenario == "codex_authentication_lost":
                    data = {"codexErrorInfo": "unauthorized"}
                elif args.scenario == "codex_usage_limit_then_success":
                    data = {"codexErrorInfo": "usageLimitExceeded"}
                else:
                    data = None
                _record(
                    args.events,
                    "codex_prompt_error_sent",
                    scenario=args.scenario,
                    data=data,
                )
                _error(
                    request_id,
                    -32603,
                    "mock Codex prompt failure",
                    data=data,
                )
                continue
            if args.scenario in {
                "claude_authentication_lost",
                "claude_rate_limit_then_success",
                "claude_uncertain_prompt_error",
            } and (
                args.scenario != "claude_rate_limit_then_success" or prompt_turn == 1
            ):
                data: object | None
                if args.scenario == "claude_authentication_lost":
                    data = {"errorKind": "authentication_failed"}
                elif args.scenario == "claude_rate_limit_then_success":
                    data = {"errorKind": "rate_limit"}
                else:
                    data = None
                _record(
                    args.events,
                    "claude_prompt_error_sent",
                    scenario=args.scenario,
                    data=data,
                )
                _error(
                    request_id,
                    -32603,
                    "mock Claude prompt failure",
                    data=data,
                )
                continue
            request_error_scenarios = {
                "act_callback_fault_then_request_error",
                "act_invalid_then_request_error",
                "act_ninth_invalid_then_request_error",
                "act_request_error_then_success",
                "act_request_error_internal_collision_then_success",
                "act_request_error_parse_collision_then_success",
                "act_request_error_transport_collision_then_success",
            }
            if args.scenario in request_error_scenarios and prompt_turn == 1:
                if args.scenario in {
                    "act_invalid_then_request_error",
                    "act_ninth_invalid_then_request_error",
                }:
                    attempts = (
                        9 if args.scenario == "act_ninth_invalid_then_request_error" else 1
                    )
                    for index in range(attempts):
                        rejected = _submit_result(
                            current_mcp_server,
                            700 + index,
                            {"value": "not-an-integer"},
                        )
                        assert rejected["result"]["isError"] is True, rejected
                        _record(
                            args.events,
                            "tool_result_received",
                            index=index,
                            is_error=True,
                            text=rejected["result"]["content"][0]["text"],
                        )
                elif args.scenario == "act_callback_fault_then_request_error":
                    callback_failed = _submit_result(
                        current_mcp_server,
                        800,
                        {"value": 2},
                    )
                    assert callback_failed["result"]["isError"] is True, callback_failed
                    _record(
                        args.events,
                        "tool_result_received",
                        is_error=True,
                        text=callback_failed["result"]["content"][0]["text"],
                    )
                _record(args.events, "prompt_request_error_sent")
                if args.scenario in {
                    "act_invalid_then_request_error",
                    "act_request_error_transport_collision_then_success",
                }:
                    _error(
                        request_id,
                        -32603,
                        "Incoming transport closed",
                        data={
                            "reason": "incoming_transport_closed",
                            "method": "session/prompt",
                        },
                    )
                elif args.scenario in {
                    "act_callback_fault_then_request_error",
                    "act_request_error_parse_collision_then_success",
                }:
                    _error(
                        request_id,
                        -32700,
                        "failed to deserialize response",
                        data={"phase": "deserialization", "json": {}},
                    )
                elif args.scenario in {
                    "act_ninth_invalid_then_request_error",
                    "act_request_error_internal_collision_then_success",
                }:
                    _error(
                        request_id,
                        -32603,
                        "mock prompt request failed",
                        data="response to `session/prompt` never received: canceled",
                    )
                else:
                    _error(request_id, -32603, "mock prompt request failed")
                continue
            if args.scenario == "act_settle_during_callback":
                response: list[dict[str, Any] | None] = [None]

                def submit_pending() -> None:
                    response[0] = _submit_result(
                        current_mcp_server,
                        350,
                        {"value": 2},
                    )

                submission = threading.Thread(target=submit_pending)
                submission.start()
                with Path("callback-started.fifo").open("rb", buffering=0) as signal:
                    assert signal.read(1) == b"1"
                _response(request_id, {"stopReason": "end_turn"})
                submission.join()
                assert response[0] is not None
                _record(
                    args.events,
                    "tool_result_received",
                    is_error=response[0]["result"]["isError"],
                    text=response[0]["result"]["content"][0]["text"],
                )
                with Path("tool-result-finished.fifo").open("wb", buffering=0) as signal:
                    signal.write(b"1")
                continue
            if args.scenario == "act_body_limits":
                invalid_message = json.dumps(
                    {
                        "jsonrpc": "2.0",
                        "id": 500,
                        "method": "tools/call",
                        "params": {
                            "name": RESULT_TOOL,
                            "arguments": {"value": {"decision": "maybe"}},
                        },
                    },
                    separators=(",", ":"),
                ).encode("utf-8")
                for index, size in enumerate(
                    (MCP_BODY_MAX_BYTES - 1, MCP_BODY_MAX_BYTES),
                    start=1,
                ):
                    body = invalid_message + b" " * (size - len(invalid_message))
                    status, response_body, _ = _post_tool_body(
                        current_mcp_server,
                        body,
                    )
                    assert status == 200
                    response = json.loads(response_body)
                    assert f"invalid call {index}/8" in response["result"]["content"][
                        0
                    ]["text"]
                oversized_response = _post_declared_oversized_body(
                    current_mcp_server,
                    MCP_BODY_MAX_BYTES + 1,
                )
                assert oversized_response.startswith(
                    b"HTTP/1.1 413 Payload Too Large\r\n"
                )
                assert b"connection: close\r\n" in oversized_response.lower()
                for index, size in enumerate(
                    (
                        MCP_BODY_MAX_BYTES - 1,
                        MCP_BODY_MAX_BYTES,
                        MCP_BODY_MAX_BYTES + 1,
                    ),
                    start=3,
                ):
                    chunked_message = invalid_message.replace(
                        b'"id":500',
                        f'"id":{500 + index}'.encode("ascii"),
                    )
                    body = chunked_message + b" " * (size - len(chunked_message))
                    midpoint = len(body) // 2
                    status, response_body, connection = _post_tool_body(
                        current_mcp_server,
                        [body[:midpoint], body[midpoint:]],
                        chunked=True,
                    )
                    if size <= MCP_BODY_MAX_BYTES:
                        assert status == 200
                        chunked_response = json.loads(response_body)
                        assert f"invalid call {index}/8" in chunked_response["result"][
                            "content"
                        ][0]["text"]
                    else:
                        assert status == 413
                        assert connection is not None
                        assert connection.lower() == "close"
                value = {"decision": "approve"}
                accepted = _submit_result(current_mcp_server, 502, value)
                assert accepted["result"]["isError"] is False
                _record(args.events, "body_limit_matrix_complete")
            elif args.scenario == "act_result_matrix":
                unauthorized_status, unauthorized = _submit_raw_tool_call(
                    current_mcp_server,
                    400,
                    arguments={"value": {"decision": "approve"}},
                    authorization="Bearer invalid-token",
                )
                assert unauthorized_status == 401 and unauthorized is None
                status, wrong_tool = _submit_raw_tool_call(
                    current_mcp_server,
                    401,
                    name="wrong_tool",
                    arguments={"value": {"decision": "approve"}},
                )
                assert status == 200 and wrong_tool is not None
                assert wrong_tool["error"]["code"] == -32602
                status, wrong_arguments = _submit_raw_tool_call(
                    current_mcp_server,
                    402,
                    arguments={
                        "value": {"decision": "approve"},
                        "extra": True,
                    },
                )
                assert status == 200 and wrong_arguments is not None
                assert wrong_arguments["error"]["code"] == -32602
                for invalid_call in range(1, 9):
                    invalid = _submit_result(
                        current_mcp_server,
                        410 + invalid_call,
                        {"decision": "maybe"},
                    )
                    assert invalid["result"]["isError"] is True
                    assert f"invalid call {invalid_call}/8" in invalid["result"][
                        "content"
                    ][0]["text"]
                value = {"decision": "approve"}
                accepted = _submit_result(current_mcp_server, 430, value)
                assert accepted["result"]["isError"] is False
                duplicate = _submit_result(
                    current_mcp_server,
                    431,
                    {"decision": "reject"},
                )
                assert duplicate["result"]["isError"] is True
                assert "already submitted" in duplicate["result"]["content"][0][
                    "text"
                ]
                _record(
                    args.events,
                    "result_matrix_complete",
                    unauthorized_status=unauthorized_status,
                )
            elif args.scenario == "act_digit_limit_integer":
                positive_digits = "9" * 5_000
                negative_digits = "-" + "9" * 5_001
                body = (
                    '{"jsonrpc":"2.0","id":440,"method":"tools/call",'
                    f'"params":{{"name":"{RESULT_TOOL}","arguments":'
                    '{"value":{"value":{"huge":'
                    + positive_digits
                    + ',"negative_huge":'
                    + negative_digits
                    + "}}}}}"
                ).encode("ascii")
                status, response_body, _ = _post_tool_body(
                    current_mcp_server,
                    body,
                )
                assert status == 200
                response = json.loads(response_body)
                assert response["result"]["isError"] is False, response
                value = {"raw_integer_digits": [5_000, 5_001]}
            elif args.scenario in {"act_submit_results", "act_submit_concurrent"}:
                assert args.results_json is not None
                values = json.loads(args.results_json)
                assert isinstance(values, list) and values
                responses: list[dict[str, Any] | None] = [None] * len(values)

                def submit(index: int, value: object) -> None:
                    responses[index] = _submit_result(
                        current_mcp_server,
                        300 + index,
                        value,
                    )

                if args.scenario == "act_submit_concurrent":
                    barrier = threading.Barrier(len(values))

                    def submit_together(index: int, value: object) -> None:
                        barrier.wait()
                        submit(index, value)

                    threads = [
                        threading.Thread(target=submit_together, args=(index, value))
                        for index, value in enumerate(values)
                    ]
                    for thread in threads:
                        thread.start()
                    for thread in threads:
                        thread.join()
                else:
                    for index, value in enumerate(values):
                        submit(index, value)
                for index, response in enumerate(responses):
                    assert response is not None
                    _record(
                        args.events,
                        "tool_result_received",
                        index=index,
                        is_error=response["result"]["isError"],
                        text=response["result"]["content"][0]["text"],
                    )
                value = values[-1]
            elif args.scenario == "act_custom_correction":
                invalid = _submit_result(
                    current_mcp_server,
                    100 + prompt_turn,
                    {"value": 3},
                )
                assert invalid["result"]["isError"] is True, invalid
                value = {"value": 4}
                accepted = _submit_result(
                    current_mcp_server,
                    200 + prompt_turn,
                    value,
                )
                assert accepted["result"]["isError"] is False, accepted
            elif args.scenario in {
                "act_accepted_non_end_turn",
                "act_request_error_then_success",
                "act_request_error_internal_collision_then_success",
                "act_request_error_parse_collision_then_success",
                "act_request_error_transport_collision_then_success",
                "codex_usage_limit_then_success",
                "claude_rate_limit_then_success",
            }:
                value = {"value": 7}
                accepted = _submit_result(
                    current_mcp_server,
                    600 + prompt_turn,
                    value,
                )
                assert accepted["result"]["isError"] is False, accepted
            elif prompt_turn == 1:
                assert "alpha" in prompt_text
                remembered_token = "alpha"
                value = {"stored": remembered_token}
            else:
                assert prompt_turn == 2
                assert remembered_token is not None
                value = {"remembered": remembered_token}
            if args.scenario in {
                "act_late_permission_after_terminal",
                "act_late_update_after_terminal",
                "act_two_turns",
                "act_ordinary_update_burst",
                "opening_crash_twice_then_ready",
                "post_ready_config_after_result",
                "act_terminal_then_permission_batch",
            }:
                tool_response = _submit_result(
                    current_mcp_server,
                    100 + prompt_turn,
                    value,
                )
                assert tool_response["result"]["isError"] is False, tool_response
            _record(args.events, "result_submitted", turn=prompt_turn, value=value)
            if args.scenario == "act_terminal_then_permission_batch":
                assert current_session_id is not None
                pending_permission_id = "permission-after-terminal-batch"
                _response_and_request_batch(
                    request_id,
                    {"stopReason": "end_turn"},
                    pending_permission_id,
                    "session/request_permission",
                    _permission_params(current_session_id),
                )
                _record(args.events, "terminal_then_permission_batch_sent")
                continue
            _response(
                request_id,
                {
                    "stopReason": (
                        "max_tokens"
                        if args.scenario == "act_accepted_non_end_turn"
                        else "end_turn"
                    )
                },
            )
            if args.scenario == "act_late_update_after_terminal":
                assert args.release is not None

                def send_late_update() -> None:
                    assert args.release is not None
                    with args.release.open("rb", buffering=0) as release:
                        assert release.read(1) == b"1"
                    _notification(
                        "session/update",
                        {
                            "sessionId": current_session_id,
                            "update": {
                                "sessionUpdate": "agent_message_chunk",
                                "content": {
                                    "type": "text",
                                    "text": "late ordinary diagnostic update",
                                },
                            },
                        },
                    )
                    _record(args.events, "late_ordinary_update_sent")

                late_update = threading.Thread(target=send_late_update)
                late_update.start()
                background_threads.append(late_update)
            if args.scenario == "act_late_permission_after_terminal":
                assert args.release is not None

                def send_late_permission() -> None:
                    assert args.release is not None
                    assert current_session_id is not None
                    with args.release.open("rb", buffering=0) as release:
                        assert release.read(1) == b"1"
                    _request(
                        "permission-after-terminal-idle",
                        "session/request_permission",
                        _permission_params(current_session_id),
                    )
                    _record(args.events, "late_permission_request_sent")

                pending_permission_id = "permission-after-terminal-idle"
                late_permission = threading.Thread(target=send_late_permission)
                late_permission.start()
                background_threads.append(late_permission)
        elif method == "session/cancel" and args.scenario in {
            "act_cancel_after_result_then_reuse",
            "act_cancel_during_callback_then_reuse",
            "act_cancel_permission_then_reuse",
            "act_cancel_then_reuse",
            "act_cancel_transport_lost",
            "act_result_rejected_then_reuse",
            "claude_cancel_end_turn_race",
            "claude_synthetic_cancel",
            "claude_synthetic_cancel_detached_descendant",
        }:
            assert params["sessionId"] == current_session_id
            assert pending_prompt_id is not None
            _record(args.events, "cancel_received", turn=1)

            if args.scenario == "act_cancel_transport_lost":
                _record(args.events, "transport_closing_after_cancel")
                return 0

            assert args.release is not None

            if args.scenario == "act_cancel_permission_then_reuse":
                pending_permission_id = "permission-after-cancel"
                _request(
                    pending_permission_id,
                    "session/request_permission",
                    _permission_params(current_session_id),
                )
                continue

            def settle_cancelled_turn(request_id: object = pending_prompt_id) -> None:
                assert args.release is not None
                with args.release.open("rb", buffering=0) as release:
                    assert release.read(1) == b"1"
                _response(
                    request_id,
                    {
                        "stopReason": (
                            "end_turn"
                            if args.scenario == "claude_cancel_end_turn_race"
                            else "cancelled"
                        )
                    },
                )
                _record(args.events, "cancel_settled", turn=1)

            settlement = threading.Thread(target=settle_cancelled_turn)
            settlement.start()
            background_threads.append(settlement)
        elif method == "session/set_mode":
            config["mode"] = params["modeId"]
            _record(
                args.events,
                "legacy_mode_applied",
                mode_id=params["modeId"],
            )
            if args.scenario == "legacy_mode_drift_before_model":
                assert current_session_id is not None
                config["mode"] = "default"
                _response_and_notification_batch(
                    request_id,
                    {},
                    "session/update",
                    {
                        "sessionId": current_session_id,
                        "update": {
                            "sessionUpdate": "current_mode_update",
                            "currentModeId": "default",
                        },
                    },
                )
                _record(args.events, "legacy_mode_drift_sent")
            else:
                _response(request_id, {})
        elif method == "session/set_config_option":
            if args.scenario == "oversized_acp_frame_configure":
                _record(args.events, "oversized_acp_frame_sent", phase="configure")
                _write_oversized_acp_frame()
                continue
            config_id = params["configId"]
            config[config_id] = params["value"]
            if args.scenario == "mode_drift_after_model" and config_id == "model":
                config["mode"] = "default"
            _record(
                args.events,
                "config_applied",
                config_id=config_id,
                value=params["value"],
            )
            invalid_domain = {
                "mode_domain_invalid": "mode",
                "model_domain_invalid": "model",
                "effort_domain_invalid": "reasoning_effort",
            }.get(args.scenario)
            config_response = {
                "configOptions": _config_options(
                    config,
                    invalid_domain=invalid_domain,
                )
            }
            if (
                config_id == "reasoning_effort"
                and args.scenario == "final_config_response_then_drift"
            ):
                assert current_session_id is not None
                drifted = dict(config)
                drifted["model"] = "default-model"
                _response_and_notification_batch(
                    request_id,
                    config_response,
                    "session/update",
                    {
                        "sessionId": current_session_id,
                        "update": {
                            "sessionUpdate": "config_option_update",
                            "configOptions": _config_options(drifted),
                        },
                    },
                )
                _record(args.events, "final_config_response_then_drift_sent")
            else:
                _response(request_id, config_response)
            if config_id == "reasoning_effort" and args.scenario in {
                "pre_ready_config_model_drift",
                "post_ready_config_exact",
                "post_ready_config_model_drift",
                "post_ready_config_model_malformed",
                "post_ready_session_update_malformed",
                "post_ready_config_after_result",
                "post_ready_config_during_turn",
            }:
                assert args.release is not None
                assert current_session_id is not None
                update_config = dict(config)
                if args.scenario in {
                    "pre_ready_config_model_drift",
                    "post_ready_config_model_drift",
                    "post_ready_config_after_result",
                    "post_ready_config_during_turn",
                }:
                    update_config["model"] = "default-model"
                config_options = _config_options(update_config)
                if args.scenario == "post_ready_config_model_malformed":
                    model_option = next(
                        option for option in config_options if option["id"] == "model"
                    )
                    model_option["currentValue"] = None

                def send_config_update(
                    session_id: str = current_session_id,
                    options: list[dict[str, object]] = config_options,
                ) -> None:
                    assert args.release is not None
                    with args.release.open("rb", buffering=0) as release:
                        assert release.read(1) == b"1"
                    if args.scenario == "post_ready_session_update_malformed":
                        _notification("session/update", {"malformed": True})
                    else:
                        _notification(
                            "session/update",
                            {
                                "sessionId": session_id,
                                "update": {
                                    "sessionUpdate": "config_option_update",
                                    "configOptions": options,
                                },
                            },
                        )
                    _record(
                        args.events,
                        (
                            "pre_ready_config_update_sent"
                            if args.scenario == "pre_ready_config_model_drift"
                            else "post_ready_config_update_sent"
                        ),
                    )

                config_update = threading.Thread(target=send_config_update)
                config_update.start()
                background_threads.append(config_update)
            if config_id == "reasoning_effort" and args.scenario in {
                "opening_eof_before_mcp_ready",
                "opening_protocol_error_before_mcp_ready",
            }:
                if args.scenario == "opening_eof_before_mcp_ready":
                    _record(args.events, "opening_stdout_closing_before_mcp_ready")
                    with _OUTPUT_LOCK:
                        os.close(sys.stdout.fileno())
                else:
                    assert args.release is not None

                    def send_protocol_error() -> None:
                        assert args.release is not None
                        with args.release.open("rb", buffering=0) as release:
                            assert release.read(1) == b"1"
                        _record(
                            args.events,
                            "opening_protocol_error_sent_before_mcp_ready",
                        )
                        with _OUTPUT_LOCK:
                            sys.stdout.buffer.write(b"{not-json}\n")
                            sys.stdout.buffer.flush()

                    protocol_error = threading.Thread(target=send_protocol_error)
                    protocol_error.start()
                    background_threads.append(protocol_error)
                continue
            if (
                args.scenario == "oversized_acp_frame_mcp_ready"
                and config_id == "reasoning_effort"
            ):
                assert args.release is not None
                _record(args.events, "mcp_ready_frame_blocked")
                while not args.release.exists():
                    time.sleep(0.01)
                _record(args.events, "oversized_acp_frame_sent", phase="mcp_ready")
                _write_oversized_acp_frame()
                continue
            discover_after = (
                args.scenario == "mcp_between_configuration" and config_id == "mode"
            ) or (
                args.scenario == "mcp_after_configuration"
                and config_id == "reasoning_effort"
            )
            if discover_after:
                assert pending_mcp_server is not None
                _discover_mcp(
                    pending_mcp_server,
                    args.events,
                    pending_mcp_invocation,
                )
                pending_mcp_server = None
        else:
            if request_id is not None:
                _error(request_id, -32601, "Method not found")

    for thread in background_threads:
        thread.join()
    _record(args.events, "stdin_closed")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
