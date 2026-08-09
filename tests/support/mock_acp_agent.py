from __future__ import annotations

import argparse
import http.client
import json
import os
import socket
import sys
import time
from pathlib import Path
from typing import Any
from urllib.parse import urlsplit


MCP_REVISION = "2025-11-25"
RESULT_TOOL = "troupe_submit_result"


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


def _response(request_id: object, result: object) -> None:
    print(
        json.dumps(
            {"jsonrpc": "2.0", "id": request_id, "result": result},
            separators=(",", ":"),
        ),
        flush=True,
    )


def _error(request_id: object, code: int, message: str) -> None:
    print(
        json.dumps(
            {
                "jsonrpc": "2.0",
                "id": request_id,
                "error": {"code": code, "message": message},
            },
            separators=(",", ":"),
        ),
        flush=True,
    )


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
    def choices(identifier: str, values_: tuple[str, ...]) -> tuple[str, ...]:
        if invalid_domain != identifier:
            return values_
        return tuple(value for value in values_ if value != values[identifier])

    options = [
        _select_option(
            "mode",
            "Mode",
            values["mode"],
            choices("mode", ("default", "agent")),
            "mode",
        ),
        _select_option(
            "model",
            "Model",
            values["model"],
            choices("model", ("default-model", "test-model")),
            "model",
        ),
        _select_option(
            "reasoning_effort",
            "Reasoning effort",
            values["reasoning_effort"],
            choices("reasoning_effort", ("medium", "max")),
            "thought_level",
        ),
    ]
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


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--events", type=Path, required=True)
    parser.add_argument("--scenario", default="ready")
    parser.add_argument("--release", type=Path)
    args = parser.parse_args()

    cwd_metadata = os.stat(".")
    _record(
        args.events,
        "process_started",
        cwd=os.getcwd(),
        cwd_dev=cwd_metadata.st_dev,
        cwd_ino=cwd_metadata.st_ino,
    )
    config = {
        "mode": "default",
        "model": "default-model",
        "reasoning_effort": "medium",
    }
    session_invocations = 0
    partial_route_connection: socket.socket | None = None
    pending_mcp_server: dict[str, Any] | None = None
    pending_mcp_invocation = 0

    for raw_line in sys.stdin:
        request = json.loads(raw_line)
        method = request.get("method")
        request_id = request.get("id")
        params = request.get("params", {})
        _record(args.events, "acp_request", method=method)

        if method == "initialize":
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
            )
        elif method == "session/new":
            session_invocations += 1
            servers = params["mcpServers"]
            assert len(servers) == 1 and servers[0]["type"] == "http"
            server = servers[0]
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
            if args.scenario == "http_10_before_discovery":
                status = _mcp_http_10_initialize(server)
                _record(args.events, "mcp_http_10_rejected", status=status)
                assert status == 505
            delayed_mcp = args.scenario in {
                "mcp_before_configuration",
                "mcp_between_configuration",
                "mcp_after_configuration",
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
                        if args.scenario in {"legacy_mode", "legacy_mode_missing"}
                        else {}
                    ),
                    "configOptions": (
                        []
                        if args.scenario == "configuration_invalid"
                        else _config_options(
                            config,
                            include_mode=args.scenario
                            not in {"legacy_mode", "legacy_mode_missing"},
                        )
                    ),
                },
            )
            if args.scenario == "mcp_before_configuration":
                assert pending_mcp_server is not None
                _discover_mcp(
                    pending_mcp_server,
                    args.events,
                    pending_mcp_invocation,
                )
                pending_mcp_server = None
        elif method == "session/set_mode":
            config["mode"] = params["modeId"]
            _record(
                args.events,
                "legacy_mode_applied",
                mode_id=params["modeId"],
            )
            _response(request_id, {})
        elif method == "session/set_config_option":
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
            _response(
                request_id,
                {
                    "configOptions": _config_options(
                        config,
                        invalid_domain=invalid_domain,
                    )
                },
            )
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
            _error(request_id, -32601, "Method not found")

    _record(args.events, "stdin_closed")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
