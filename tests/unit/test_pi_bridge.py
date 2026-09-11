from __future__ import annotations

import json
import os
import select
import shutil
import subprocess
import time
from collections.abc import Callable
from pathlib import Path
from typing import BinaryIO

import pytest

ROOT = Path(__file__).resolve().parents[2]
SHIM = (
    ROOT / "rust" / "crates" / "troupe-agent-runtime" / "assets" / "pi" / "acp-shim.mjs"
)
EXTENSION = (
    ROOT
    / "rust"
    / "crates"
    / "troupe-agent-runtime"
    / "assets"
    / "pi"
    / "result-extension.mjs"
)


FAKE_PI = r"""#!/usr/bin/env node
import process from "node:process";

let buffer = "";
function send(value) {
  process.stdout.write(`${JSON.stringify(value)}\n`);
}
process.stdin.on("data", (chunk) => {
  buffer += chunk.toString("utf8");
  while (true) {
    const newline = buffer.indexOf("\n");
    if (newline < 0) break;
    const line = buffer.slice(0, newline);
    buffer = buffer.slice(newline + 1);
    if (!line) continue;
    const command = JSON.parse(line);
    if (command.type === "get_state") {
      send({
        id: command.id,
        type: "response",
        success: true,
        data: { model: { provider: "deepseek", id: "deepseek-flash" } },
      });
    } else if (command.type === "prompt") {
      send({ id: command.id, type: "response", success: true });
      send({
        type: "message_update",
        assistantMessageEvent: { type: "text_delta", delta: "assistant text" },
      });
      send({ type: "agent_settled" });
    }
  }
});
"""

FAKE_PI_AUTH_FAILURE = r"""#!/usr/bin/env node
import process from "node:process";

let buffer = "";
function send(value) {
  process.stdout.write(`${JSON.stringify(value)}\n`);
}
process.stdin.on("data", (chunk) => {
  buffer += chunk.toString("utf8");
  while (true) {
    const newline = buffer.indexOf("\n");
    if (newline < 0) break;
    const line = buffer.slice(0, newline);
    buffer = buffer.slice(newline + 1);
    if (!line) continue;
    const command = JSON.parse(line);
    if (command.type === "get_state") {
      send({
        id: command.id,
        type: "response",
        success: true,
        data: { model: { provider: "deepseek", id: "deepseek-flash" } },
      });
    } else if (command.type === "prompt") {
      send({
        id: command.id,
        type: "response",
        success: false,
        error: "No API key found for deepseek",
      });
    }
  }
});
"""

FAKE_PI_CAPACITY_FAILURE = r"""#!/usr/bin/env node
import process from "node:process";

let buffer = "";
function send(value) {
  process.stdout.write(`${JSON.stringify(value)}\n`);
}
process.stdin.on("data", (chunk) => {
  buffer += chunk.toString("utf8");
  while (true) {
    const newline = buffer.indexOf("\n");
    if (newline < 0) break;
    const line = buffer.slice(0, newline);
    buffer = buffer.slice(newline + 1);
    if (!line) continue;
    const command = JSON.parse(line);
    if (command.type === "get_state") {
      send({
        id: command.id,
        type: "response",
        success: true,
        data: { model: { provider: "deepseek", id: "deepseek-flash" } },
      });
    } else if (command.type === "prompt") {
      send({ id: command.id, type: "response", success: true });
      send({
        type: "message_end",
        message: {
          stopReason: "error",
          errorMessage: "Selected model is at capacity. Please try a different model",
        },
      });
      send({ type: "agent_settled" });
    }
  }
});
"""

FAKE_PI_TOOL_DELTA = r"""#!/usr/bin/env node
import process from "node:process";

let buffer = "";
function send(value) {
  process.stdout.write(`${JSON.stringify(value)}\n`);
}
process.stdin.on("data", (chunk) => {
  buffer += chunk.toString("utf8");
  while (true) {
    const newline = buffer.indexOf("\n");
    if (newline < 0) break;
    const line = buffer.slice(0, newline);
    buffer = buffer.slice(newline + 1);
    if (!line) continue;
    const command = JSON.parse(line);
    if (command.type === "get_state") {
      send({
        id: command.id,
        type: "response",
        success: true,
        data: { model: { provider: "deepseek", id: "deepseek-flash" } },
      });
    } else if (command.type === "prompt") {
      send({ id: command.id, type: "response", success: true });
      send({
        type: "message_update",
        assistantMessageEvent: {
          type: "toolcall_start",
          id: "call-1",
          toolName: "echo",
        },
      });
      send({
        type: "message_update",
        assistantMessageEvent: { type: "toolcall_delta", id: "call-1", delta: '{"alpha":' },
      });
      send({
        type: "message_update",
        assistantMessageEvent: { type: "toolcall_delta", id: "call-1", delta: '"beta"}' },
      });
      send({ type: "tool_execution_start", toolCallId: "call-1", toolName: "echo" });
      send({
        type: "tool_execution_end",
        toolCallId: "call-1",
        result: { content: [{ type: "text", text: "ok" }] },
        isError: false,
      });
      send({ type: "agent_settled" });
    }
  }
});
"""


class _JsonLineReader:
    def __init__(self, stream: BinaryIO) -> None:
        self._stream = stream
        self._buffer = bytearray()

    def read_until(
        self,
        predicate: Callable[[dict[str, object]], bool],
        *,
        timeout: float = 5.0,
    ) -> list[dict[str, object]]:
        records: list[dict[str, object]] = []
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            newline = self._buffer.find(b"\n")
            if newline >= 0:
                line = bytes(self._buffer[:newline])
                del self._buffer[: newline + 1]
                if not line:
                    continue
                record = json.loads(line)
                assert isinstance(record, dict)
                records.append(record)
                if predicate(record):
                    return records
                continue
            remaining = deadline - time.monotonic()
            ready, _, _ = select.select([self._stream], [], [], remaining)
            if not ready:
                break
            chunk = os.read(self._stream.fileno(), 64 * 1024)
            if not chunk:
                break
            self._buffer.extend(chunk)
        raise AssertionError(
            f"timed out waiting for bridge record; received {records!r}"
        )


@pytest.mark.skipif(
    shutil.which("node") is None, reason="Node.js is required for the Pi bridge test"
)
def test_pi_bridge_waits_for_agent_settled_before_finishing_prompt(
    tmp_path: Path,
) -> None:
    fake_pi = tmp_path / "fake-pi.mjs"
    fake_pi.write_text(FAKE_PI, encoding="utf-8")
    fake_pi.chmod(0o700)
    process = subprocess.Popen(
        [
            "node",
            str(SHIM),
            "--pi-command",
            str(fake_pi),
            "--extension",
            str(EXTENSION),
            "--model",
            "deepseek-flash",
        ],
        cwd=ROOT,
        stdin=subprocess.PIPE,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
    )
    assert process.stdin is not None
    assert process.stdout is not None
    reader = _JsonLineReader(process.stdout)

    def send(request: dict[str, object]) -> None:
        process.stdin.write((json.dumps(request) + "\n").encode("utf-8"))
        process.stdin.flush()

    try:
        send({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {}})
        initialize_records = reader.read_until(lambda record: record.get("id") == 1)
        initialize_response = initialize_records[-1]
        assert (
            initialize_response["result"]["agentCapabilities"]["loadSession"] is False
        )
        assert initialize_response["result"]["agentCapabilities"][
            "mcpCapabilities"
        ] == {"http": True}
        send(
            {
                "jsonrpc": "2.0",
                "id": 2,
                "method": "session/new",
                "params": {
                    "mcpServers": [
                        {
                            "type": "http",
                            "name": "troupe-result",
                            "url": "http://127.0.0.1:1/mcp",
                            "headers": [
                                {"name": "Authorization", "value": "Bearer test"}
                            ],
                        }
                    ]
                },
            }
        )
        session_records = reader.read_until(
            lambda record: record.get("id") == 2 and "result" in record,
        )
        session_response = session_records[-1]
        session_id = session_response["result"]["sessionId"]

        for request_id, config_id, value in (
            (3, "mode", "default"),
            (4, "model", "deepseek-flash"),
        ):
            send(
                {
                    "jsonrpc": "2.0",
                    "id": request_id,
                    "method": "session/set_config_option",
                    "params": {
                        "sessionId": session_id,
                        "configId": config_id,
                        "value": value,
                    },
                }
            )
            reader.read_until(
                lambda record, request_id=request_id: record.get("id") == request_id
            )

        send(
            {
                "jsonrpc": "2.0",
                "id": 5,
                "method": "session/prompt",
                "params": {
                    "sessionId": session_id,
                    "prompt": [{"type": "text", "text": "Return a result."}],
                },
            }
        )
        prompt_records = reader.read_until(lambda record: record.get("id") == 5)
        update_index = next(
            index
            for index, record in enumerate(prompt_records)
            if record.get("method") == "session/update"
        )
        response_index = next(
            index
            for index, record in enumerate(prompt_records)
            if record.get("id") == 5
        )
        assert update_index < response_index
        assert (
            prompt_records[update_index]["params"]["update"]["content"]["text"]
            == "assistant text"
        )
    finally:
        process.stdin.close()
        process.wait(timeout=5)


def _configured_bridge(
    tmp_path: Path,
    source: str,
) -> tuple[
    subprocess.Popen[bytes], _JsonLineReader, str, Callable[[dict[str, object]], None]
]:
    fake_pi = tmp_path / "fake-pi.mjs"
    fake_pi.write_text(source, encoding="utf-8")
    fake_pi.chmod(0o700)
    process = subprocess.Popen(
        [
            "node",
            str(SHIM),
            "--pi-command",
            str(fake_pi),
            "--extension",
            str(EXTENSION),
            "--model",
            "deepseek-flash",
        ],
        cwd=ROOT,
        stdin=subprocess.PIPE,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
    )
    assert process.stdin is not None
    assert process.stdout is not None
    reader = _JsonLineReader(process.stdout)

    def send(request: dict[str, object]) -> None:
        process.stdin.write((json.dumps(request) + "\n").encode("utf-8"))
        process.stdin.flush()

    send({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {}})
    reader.read_until(lambda record: record.get("id") == 1)
    send(
        {
            "jsonrpc": "2.0",
            "id": 2,
            "method": "session/new",
            "params": {
                "mcpServers": [
                    {
                        "type": "http",
                        "name": "troupe-result",
                        "url": "http://127.0.0.1:1/mcp",
                        "headers": [{"name": "Authorization", "value": "Bearer test"}],
                    }
                ]
            },
        }
    )
    session_response = reader.read_until(
        lambda record: record.get("id") == 2 and "result" in record,
    )[-1]
    session_id = session_response["result"]["sessionId"]
    for request_id, config_id, value in (
        (3, "mode", "default"),
        (4, "model", "deepseek-flash"),
    ):
        send(
            {
                "jsonrpc": "2.0",
                "id": request_id,
                "method": "session/set_config_option",
                "params": {
                    "sessionId": session_id,
                    "configId": config_id,
                    "value": value,
                },
            }
        )
        reader.read_until(
            lambda record, request_id=request_id: record.get("id") == request_id
        )
    return process, reader, session_id, send


@pytest.mark.skipif(
    shutil.which("node") is None, reason="Node.js is required for the Pi bridge test"
)
def test_pi_bridge_classifies_provider_authentication_failure_without_raw_error(
    tmp_path: Path,
) -> None:
    process, reader, session_id, send = _configured_bridge(
        tmp_path, FAKE_PI_AUTH_FAILURE
    )
    assert process.stdin is not None
    try:
        send(
            {
                "jsonrpc": "2.0",
                "id": 5,
                "method": "session/prompt",
                "params": {
                    "sessionId": session_id,
                    "prompt": [{"type": "text", "text": "hello"}],
                },
            }
        )
        response = reader.read_until(lambda record: record.get("id") == 5)[-1]
        assert response["error"]["code"] == -32000
        assert response["error"]["data"] == {
            "piErrorKind": "auth",
            "provider": "deepseek",
            "model": "deepseek-flash",
            "reason": "authentication_failed",
        }
        assert "No API key" not in json.dumps(response)
    finally:
        process.stdin.close()
        process.wait(timeout=5)


@pytest.mark.skipif(
    shutil.which("node") is None, reason="Node.js is required for the Pi bridge test"
)
def test_pi_bridge_classifies_capacity_failure_as_provider_error_without_raw_error(
    tmp_path: Path,
) -> None:
    process, reader, session_id, send = _configured_bridge(
        tmp_path, FAKE_PI_CAPACITY_FAILURE
    )
    assert process.stdin is not None
    try:
        send(
            {
                "jsonrpc": "2.0",
                "id": 5,
                "method": "session/prompt",
                "params": {
                    "sessionId": session_id,
                    "prompt": [{"type": "text", "text": "hello"}],
                },
            }
        )
        response = reader.read_until(lambda record: record.get("id") == 5)[-1]
        assert response["error"]["code"] == -32603
        assert response["error"]["data"] == {
            "piErrorKind": "provider",
            "provider": "deepseek",
            "model": "deepseek-flash",
            "reason": "provider_request_failed",
        }
        assert "at capacity" not in json.dumps(response)
    finally:
        process.stdin.close()
        process.wait(timeout=5)


@pytest.mark.skipif(
    shutil.which("node") is None, reason="Node.js is required for the Pi bridge test"
)
def test_pi_bridge_reassembles_tool_argument_deltas_before_emitting_tool_call(
    tmp_path: Path,
) -> None:
    process, reader, session_id, send = _configured_bridge(tmp_path, FAKE_PI_TOOL_DELTA)
    assert process.stdin is not None
    try:
        send(
            {
                "jsonrpc": "2.0",
                "id": 5,
                "method": "session/prompt",
                "params": {
                    "sessionId": session_id,
                    "prompt": [{"type": "text", "text": "use the tool"}],
                },
            }
        )
        records = reader.read_until(lambda record: record.get("id") == 5)
        tool_update = next(
            record["params"]["update"]
            for record in records
            if record.get("method") == "session/update"
            and record["params"]["update"].get("sessionUpdate") == "tool_call"
        )
        assert tool_update["rawInput"] == {"alpha": "beta"}
    finally:
        process.stdin.close()
        process.wait(timeout=5)


@pytest.mark.skipif(
    shutil.which("node") is None, reason="Node.js is required for the Pi bridge test"
)
def test_pi_bridge_rejects_image_prompt_blocks(tmp_path: Path) -> None:
    process, reader, session_id, send = _configured_bridge(tmp_path, FAKE_PI)
    assert process.stdin is not None
    try:
        send(
            {
                "jsonrpc": "2.0",
                "id": 5,
                "method": "session/prompt",
                "params": {
                    "sessionId": session_id,
                    "prompt": [
                        {"type": "text", "text": "inspect this"},
                        {"type": "image", "mimeType": "image/png", "data": "AA=="},
                    ],
                },
            }
        )
        response = reader.read_until(lambda record: record.get("id") == 5)[-1]
        assert response["error"]["code"] == -32602
        assert response["error"]["message"] == "Pi bridge accepts text prompts only"
    finally:
        process.stdin.close()
        process.wait(timeout=5)
