#!/usr/bin/env python3
"""Run the real-CLI diagnostics happy-path matrix."""

from __future__ import annotations

import argparse
import http.client
import importlib.util
import json
import os
import re
import selectors
import shutil
import signal
import subprocess
import sys
import tempfile
import threading
import time
from dataclasses import dataclass
from pathlib import Path
from typing import Any, BinaryIO, Iterable
from urllib.parse import urlencode, urljoin, urlsplit

import oracle


ROOT = Path(__file__).resolve().parents[3]
FIXTURE = Path(__file__).with_name("fixture_production")
MOCK = Path(__file__).with_name("mock_acp.py")
READY_PREFIX = b"troupe: diagnostic ready "
ARCHIVE_READY_PREFIX = b"troupe: diagnostic archive ready "
TIMEOUT = 30.0


@dataclass(frozen=True)
class MatrixCase:
    identifier: str
    provider: str
    mcp_revision: str
    capture_tool_payloads: bool
    usage_availability: str


def _exact_keys(value: dict[str, Any], expected: set[str], context: str) -> None:
    oracle.require(set(value) == expected, f"{context} keys differ: {sorted(value)}")


def _load_matrix(path: Path) -> list[MatrixCase]:
    value = json.loads(path.read_text(encoding="utf-8"))
    oracle.require(isinstance(value, dict), "matrix must be a JSON object")
    _exact_keys(value, {"schema_version", "cases"}, "matrix")
    oracle.require(value["schema_version"] == 1, "unsupported matrix schema")
    rows = value["cases"]
    oracle.require(isinstance(rows, list) and rows, "matrix cases must be non-empty")
    cases: list[MatrixCase] = []
    for index, row in enumerate(rows):
        oracle.require(isinstance(row, dict), f"matrix case {index} is not an object")
        _exact_keys(
            row,
            {
                "id",
                "provider",
                "mcp_revision",
                "capture_tool_payloads",
                "usage_availability",
            },
            f"matrix case {index}",
        )
        oracle.require(row["provider"] in {"codex", "claude", "kimi"}, "bad provider")
        oracle.require(
            row["usage_availability"] in {"available", "unavailable"},
            "bad usage availability",
        )
        oracle.require(
            type(row["capture_tool_payloads"]) is bool,
            "capture_tool_payloads must be boolean",
        )
        cases.append(
            MatrixCase(
                identifier=row["id"],
                provider=row["provider"],
                mcp_revision=row["mcp_revision"],
                capture_tool_payloads=row["capture_tool_payloads"],
                usage_availability=row["usage_availability"],
            )
        )
    identifiers = [case.identifier for case in cases]
    oracle.require(len(identifiers) == len(set(identifiers)), "duplicate matrix case id")
    oracle.require(
        {case.provider for case in cases} == {"codex", "claude", "kimi"},
        "matrix must qualify all three providers",
    )
    oracle.require(
        {case.capture_tool_payloads for case in cases} == {False, True},
        "matrix must cover capture on and off",
    )
    return cases


def _environment(import_marker: Path | None = None) -> dict[str, str]:
    environment = dict(os.environ)
    for name in list(environment):
        folded = name.casefold()
        if folded in {
            "all_proxy",
            "http_proxy",
            "https_proxy",
            "no_proxy",
        }:
            environment.pop(name)
    environment["PYTHONDONTWRITEBYTECODE"] = "1"
    if import_marker is not None:
        environment["TROUPE_DIAGNOSTICS_E2E_IMPORT_MARKER"] = str(import_marker)
    return environment


def _console() -> Path:
    override = os.environ.get("TROUPE_E2E_CONSOLE")
    candidate = Path(override) if override is not None else None
    if candidate is None:
        resolved = shutil.which("troupe")
        candidate = Path(resolved) if resolved is not None else None
    oracle.require(candidate is not None and candidate.is_file(), "troupe console is unavailable")
    return candidate.resolve()


def _readline(pipe: BinaryIO, process: subprocess.Popen[bytes], timeout: float) -> bytes:
    selector = selectors.DefaultSelector()
    selector.register(pipe, selectors.EVENT_READ)
    try:
        deadline = time.monotonic() + timeout
        while True:
            remaining = deadline - time.monotonic()
            oracle.require(remaining > 0, "timed out waiting for child readiness")
            if selector.select(remaining):
                line = pipe.readline()
                oracle.require(line, f"child exited before readiness with {process.poll()}")
                return line
    finally:
        selector.close()


def _wait_file(path: Path, process: subprocess.Popen[bytes], context: str) -> None:
    deadline = time.monotonic() + TIMEOUT
    while not path.is_file():
        oracle.require(process.poll() is None, f"Production exited before {context}")
        oracle.require(time.monotonic() < deadline, f"timed out waiting for {context}")
        time.sleep(0.01)


def _url_path(base_url: str, suffix: str) -> tuple[str, int, str]:
    parsed = urlsplit(base_url)
    oracle.require(parsed.scheme == "http", "diagnostic URL is not HTTP")
    oracle.require(parsed.hostname is not None and parsed.port is not None, "bad URL")
    prefix = parsed.path.rstrip("/")
    return parsed.hostname, parsed.port, f"{prefix}{suffix}"


def _http_bytes(
    base_url: str,
    suffix: str,
    *,
    accept: str,
) -> tuple[int, dict[str, str], bytes]:
    host, port, path = _url_path(base_url, suffix)
    connection = http.client.HTTPConnection(host, port, timeout=TIMEOUT)
    try:
        connection.request("GET", path, headers={"Accept": accept, "Connection": "close"})
        response = connection.getresponse()
        headers = {name.lower(): value for name, value in response.getheaders()}
        return response.status, headers, response.read()
    finally:
        connection.close()


def _http_json(base_url: str, suffix: str) -> dict[str, Any]:
    status, headers, body = _http_bytes(base_url, suffix, accept="application/json")
    oracle.require(status == 200, f"GET {suffix} returned {status}: {body[:500]!r}")
    oracle.require(
        headers.get("content-type", "").startswith("application/json"),
        f"GET {suffix} returned a non-JSON content type",
    )
    value = json.loads(body)
    oracle.require(isinstance(value, dict), f"GET {suffix} did not return an object")
    return value


def _run_cli(
    console: Path,
    arguments: Iterable[str],
    *,
    environment: dict[str, str],
    expected: int = 0,
) -> subprocess.CompletedProcess[bytes]:
    result = subprocess.run(
        [str(console), *arguments],
        cwd=ROOT,
        env=environment,
        stdin=subprocess.DEVNULL,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        timeout=TIMEOUT,
    )
    oracle.require(
        result.returncode == expected,
        f"CLI exit {result.returncode}, expected {expected}: "
        f"{result.stderr.decode(errors='replace')}",
    )
    return result


def _canonical_cli_json(result: subprocess.CompletedProcess[bytes]) -> dict[str, Any]:
    oracle.require(result.stderr == b"", f"unexpected CLI stderr: {result.stderr!r}")
    value = json.loads(result.stdout)
    oracle.require(isinstance(value, dict), "CLI machine output is not an object")
    encoded = json.dumps(value, separators=(",", ":")).encode() + b"\n"
    oracle.require(result.stdout == encoded, "CLI JSON is not canonical one-line output")
    return value


def _jsonl_stdout(result: subprocess.CompletedProcess[bytes]) -> list[dict[str, Any]]:
    oracle.require(result.stderr == b"", f"unexpected CLI stderr: {result.stderr!r}")
    rows = [json.loads(line) for line in result.stdout.decode().splitlines()]
    oracle.require(all(isinstance(row, dict) for row in rows), "bad CLI JSONL row")
    return rows


class SseCapture:
    def __init__(
        self,
        base_url: str,
        after: int,
        stop_after_events: int | None,
        *,
        stop_after_ready: bool = False,
    ) -> None:
        self.base_url = base_url
        self.after = after
        self.stop_after_events = stop_after_events
        self.stop_after_ready = stop_after_ready
        self.frames: list[dict[str, Any]] = []
        self.error: BaseException | None = None
        self.ready = threading.Event()
        self.finished = threading.Event()
        self._thread = threading.Thread(target=self._run, daemon=True)

    def start(self) -> None:
        self._thread.start()
        oracle.require(self.ready.wait(TIMEOUT), "SSE did not produce stream_ready")
        if self.error is not None:
            raise self.error

    def wait(self) -> list[dict[str, Any]]:
        oracle.require(self.finished.wait(TIMEOUT), "SSE stream did not finish")
        self._thread.join(timeout=1)
        if self.error is not None:
            raise self.error
        return self.frames

    def _run(self) -> None:
        connection: http.client.HTTPConnection | None = None
        try:
            host, port, path = _url_path(
                self.base_url, f"/api/v1/events?after={self.after}"
            )
            connection = http.client.HTTPConnection(host, port, timeout=TIMEOUT)
            connection.request(
                "GET",
                path,
                headers={"Accept": "text/event-stream", "Connection": "close"},
            )
            response = connection.getresponse()
            oracle.require(response.status == 200, f"SSE returned {response.status}")
            frame: dict[str, str] = {}
            event_count = 0
            while True:
                raw = response.readline()
                if not raw:
                    break
                line = raw.decode("utf-8").rstrip("\r\n")
                if line:
                    name, separator, value = line.partition(":")
                    oracle.require(separator == ":", f"invalid SSE line: {line!r}")
                    frame[name] = value.lstrip(" ")
                    continue
                if not frame:
                    continue
                event_name = frame.get("event")
                decoded: dict[str, Any] = {
                    "event": event_name,
                    "id": frame.get("id"),
                    "data": json.loads(frame["data"]),
                }
                self.frames.append(decoded)
                frame = {}
                if event_name == "stream_ready":
                    self.ready.set()
                    if self.stop_after_ready:
                        break
                elif event_name == "diagnostic_event":
                    event_count += 1
                    if (
                        self.stop_after_events is not None
                        and event_count >= self.stop_after_events
                    ):
                        break
                elif event_name == "stream_closed":
                    break
        except BaseException as error:
            self.error = error
            self.ready.set()
        finally:
            if connection is not None:
                connection.close()
            self.finished.set()


def _assert_sse(frames: list[dict[str, Any]], run_id: str, after: int) -> list[dict[str, Any]]:
    oracle.require(frames and frames[0]["event"] == "stream_ready", "missing stream_ready")
    oracle.require(frames[0]["data"]["run_id"] == run_id, "SSE Run mismatch")
    events = [frame["data"] for frame in frames if frame["event"] == "diagnostic_event"]
    sequences = [int(event["sequence"]) for event in events]
    oracle.require(all(sequence > after for sequence in sequences), "SSE replay crossed cursor")
    oracle.require(sequences == sorted(set(sequences)), "SSE sequence is not ordered")
    return events


def _decode_perfetto(path: Path) -> tuple[dict[str, Any], dict[str, Any]]:
    decoder_path = ROOT / "tests" / "perfetto" / "decode" / "decoder.py"
    spec = importlib.util.spec_from_file_location("troupe_e2e_perfetto_decoder", decoder_path)
    oracle.require(spec is not None and spec.loader is not None, "cannot load decoder")
    module = importlib.util.module_from_spec(spec)
    sys.modules[spec.name] = module
    spec.loader.exec_module(module)
    trace = module.decode_trace(path.read_bytes())
    module.validate_trace(trace)
    return trace, module.summarize_trace(trace)


def _prepare_package(case_root: Path) -> Path:
    package = case_root / "fixture_production"
    shutil.copytree(FIXTURE, package, ignore=shutil.ignore_patterns(".troupe", "__pycache__"))
    os.replace(package / "__init__.py", package / "production.py")
    (package / "__init__.py").write_text("", encoding="ascii")
    return package


def _start_follow(
    console: Path,
    base_url: str,
    after: int,
    environment: dict[str, str],
) -> subprocess.Popen[bytes]:
    return subprocess.Popen(
        [
            str(console),
            "diagnostic",
            "events",
            "--url",
            base_url,
            "--after",
            str(after),
            "--follow",
            "--format",
            "jsonl",
        ],
        cwd=ROOT,
        env=environment,
        stdin=subprocess.DEVNULL,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
    )


def _interrupt(process: subprocess.Popen[bytes], expected: int) -> tuple[bytes, bytes]:
    oracle.require(process.poll() is None, "child exited before interrupt")
    process.send_signal(signal.SIGINT)
    stdout, stderr = process.communicate(timeout=TIMEOUT)
    oracle.require(
        process.returncode == expected,
        f"interrupted child exited {process.returncode}, expected {expected}: {stderr!r}",
    )
    return stdout, stderr


def _stop_child(process: subprocess.Popen[bytes]) -> None:
    if process.poll() is not None:
        return
    process.send_signal(signal.SIGINT)
    try:
        process.communicate(timeout=5)
    except subprocess.TimeoutExpired:
        process.kill()
        process.communicate()


def _wait_pipe_readable(
    pipe: BinaryIO,
    process: subprocess.Popen[bytes],
    context: str,
) -> None:
    selector = selectors.DefaultSelector()
    selector.register(pipe, selectors.EVENT_READ)
    try:
        deadline = time.monotonic() + TIMEOUT
        while True:
            oracle.require(process.poll() is None, f"child exited before {context}")
            remaining = deadline - time.monotonic()
            oracle.require(remaining > 0, f"timed out waiting for {context}")
            if selector.select(min(remaining, 0.1)):
                return
    finally:
        selector.close()


def _active_cli_checks(
    console: Path,
    package: Path,
    base_url: str,
    run_id: str,
    environment: dict[str, str],
) -> tuple[dict[str, Any], dict[str, Any], list[dict[str, Any]]]:
    http_status = _http_json(base_url, "/api/v1/status")
    http_snapshot = _http_json(base_url, "/api/v1/snapshot")
    http_events = _http_json(base_url, "/api/v1/events?after=0")
    latest_snapshot = http_snapshot
    latest_events = http_events["events"]
    for target in (
        ("--production", str(package)),
        ("--url", base_url),
    ):
        status = _canonical_cli_json(
            _run_cli(
                console,
                ["diagnostic", "status", *target, "--format", "json"],
                environment=environment,
            )
        )
        oracle.require(status["run_id"] == run_id, "CLI status Run mismatch")
        oracle.require(status["source"] == "active", "CLI status did not resolve active Run")
        oracle.require(status["security_scope"] == "trusted_network", "scope drifted")
        snapshot = _canonical_cli_json(
            _run_cli(
                console,
                ["diagnostic", "snapshot", *target, "--format", "json"],
                environment=environment,
            )
        )
        snapshot_watermark = int(snapshot["watermark_sequence"])
        latest_watermark = int(latest_snapshot["watermark_sequence"])
        if snapshot_watermark == latest_watermark:
            oracle.require(snapshot == latest_snapshot, "CLI/HTTP snapshot drift")
        else:
            oracle.require(
                snapshot_watermark > latest_watermark,
                "live snapshot watermark moved backwards",
            )
            latest_snapshot = snapshot
        events = _jsonl_stdout(
            _run_cli(
                console,
                ["diagnostic", "events", *target, "--after", "0", "--format", "jsonl"],
                environment=environment,
            )
        )
        common = min(len(events), len(latest_events))
        oracle.require(
            events[:common] == latest_events[:common],
            "CLI/HTTP event prefix drift",
        )
        if len(events) > len(latest_events):
            latest_events = events
    return http_status, latest_snapshot, latest_events


def _view_checks(
    base_url: str,
    run_id: str,
    stage_binding: dict[str, Any],
) -> dict[str, dict[str, Any]]:
    catalog = _http_json(base_url, "/api/v1/views")
    oracle.assert_view_catalog(catalog, run_id)
    snapshot = _http_json(base_url, "/api/v1/snapshot")
    captured_end = int(snapshot["state"]["through_elapsed_ns"]) + 1
    responses: dict[str, dict[str, Any]] = {}
    for view in catalog["views"]:
        query = {"view_id": view["id"]}
        if view["time_range"] == "viewport":
            query.update(
                viewport_start_ns="0",
                viewport_end_ns=str(captured_end),
            )
        response = _http_json(base_url, "/api/v1/views?" + urlencode(query))
        oracle.assert_view_response(response, view["renderer"], run_id)
        responses[view["renderer"]] = response
    series = responses["time_series"]
    binding = series["binding"]
    oracle.require(
        int(binding["captured_watermark"]) > int(stage_binding["captured_watermark"]),
        "view watermark did not advance after live commit",
    )
    end = int(binding["captured_elapsed_end_ns"])
    exact = {
        "view_id": "queue_depth",
        "captured_watermark": binding["captured_watermark"],
        "captured_elapsed_end_ns": binding["captured_elapsed_end_ns"],
        "viewport_start_ns": "0",
        "viewport_end_ns": str(end),
    }
    full = _http_json(base_url, "/api/v1/views?" + urlencode(exact))
    half_query = dict(exact)
    half_query["viewport_end_ns"] = str(max(1, end // 2))
    half = _http_json(base_url, "/api/v1/views?" + urlencode(half_query))
    oracle.require(full["binding"] != half["binding"], "viewport bindings were mixed")
    oracle.require(
        full["bucket_width_ns"] != half["bucket_width_ns"],
        "derived TimeSeries width did not change with range",
    )
    points = [point for item in full.get("series", []) for point in item["points"]]
    oracle.require(any(point["value"] is None for point in points), "no explicit empty bucket")
    oracle.require(any(point["partial"] for point in points), "no partial terminal bucket")
    oracle.require("coverage" in full, "TimeSeries omitted coverage")
    return responses


def _asset_checks(base_url: str) -> None:
    status, headers, body = _http_bytes(base_url, "/", accept="text/html")
    oracle.require(status == 200, f"diagnostic root returned {status}")
    oracle.require(headers.get("content-type", "").startswith("text/html"), "root is not HTML")
    text = body.decode("utf-8")
    assets = sorted(set(re.findall(r'(?:src|href)="([^"]+/assets/[^"]+)"', text)))
    oracle.require(assets, "diagnostic shell contains no hashed assets")
    parsed = urlsplit(base_url)
    prefix = parsed.path.rstrip("/")
    for asset in assets:
        resolved_path = urlsplit(urljoin(base_url.rstrip("/") + "/", asset)).path
        suffix = (
            resolved_path[len(prefix) :]
            if prefix and resolved_path.startswith(prefix)
            else resolved_path
        )
        status, asset_headers, payload = _http_bytes(base_url, suffix, accept="*/*")
        oracle.require(status == 200 and payload, f"asset {asset} is unavailable")
        oracle.require("content-type" in asset_headers, f"asset {asset} has no content type")


def _dump(
    console: Path,
    target: tuple[str, str],
    output: Path,
    environment: dict[str, str],
) -> dict[str, Any]:
    result = _run_cli(
        console,
        ["diagnostic", "dump", *target, "--output", str(output)],
        environment=environment,
    )
    oracle.require(result.stdout == b"", "dump wrote machine data to stdout")
    line = result.stderr.strip()
    prefix = b"troupe: diagnostic dump "
    oracle.require(line.startswith(prefix), f"bad dump report: {line!r}")
    report = json.loads(line.removeprefix(prefix))
    oracle.require(report["publication"] == "published", "dump was not published")
    oracle.require(output.is_file() and output.stat().st_size > 0, "dump file is empty")
    return report


def _archive_checks(
    console: Path,
    package: Path,
    archive: Path,
    base_run_id: str,
    active_prefix: list[dict[str, Any]],
    import_marker: Path,
    environment: dict[str, str],
    case_root: Path,
    children: list[subprocess.Popen[bytes]],
) -> list[dict[str, Any]]:
    initial_imports = import_marker.read_text(encoding="ascii")
    target = ("--archive", str(archive))
    status = _canonical_cli_json(
        _run_cli(
            console,
            ["diagnostic", "status", *target, "--format", "json"],
            environment=environment,
        )
    )
    oracle.require(status["source"] == "archive", "archive status source drifted")
    snapshot = _canonical_cli_json(
        _run_cli(
            console,
            ["diagnostic", "snapshot", *target, "--format", "json"],
            environment=environment,
        )
    )
    oracle.require(snapshot["run_id"] == base_run_id, "archive snapshot Run mismatch")
    events = _jsonl_stdout(
        _run_cli(
            console,
            ["diagnostic", "events", *target, "--after", "0", "--format", "jsonl"],
            environment=environment,
        )
    )
    oracle.assert_dense_events(events, base_run_id)
    oracle.require(
        events[: len(active_prefix)] == active_prefix,
        "archive does not preserve the active canonical prefix",
    )
    archive_trace = case_root / "archive.pftrace"
    report = _dump(console, target, archive_trace, environment)
    _, summary = _decode_perfetto(archive_trace)
    oracle.assert_perfetto_summary(summary, base_run_id, int(report["exported_through"]))

    serve = subprocess.Popen(
        [str(console), "diagnostic", "serve", *target, "--port", "0"],
        cwd=ROOT,
        env=environment,
        stdin=subprocess.DEVNULL,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
    )
    children.append(serve)
    assert serve.stderr is not None
    line = _readline(serve.stderr, serve, TIMEOUT).rstrip(b"\n")
    oracle.require(line.startswith(ARCHIVE_READY_PREFIX), f"bad archive ready: {line!r}")
    locator = json.loads(line.removeprefix(ARCHIVE_READY_PREFIX))
    oracle.require(locator["run_id"] == base_run_id, "archive server Run mismatch")
    oracle.require(locator["local_url"].startswith("http://127.0.0.1:"), "serve is not loopback")
    archive_url = locator["local_url"]
    _asset_checks(archive_url)
    oracle.require(_http_json(archive_url, "/api/v1/status")["source"] == "archive", "serve status")
    oracle.require(
        _http_json(archive_url, "/api/v1/events?after=0")["events"] == events,
        "archive serve event drift",
    )
    oracle.assert_view_catalog(_http_json(archive_url, "/api/v1/views"), base_run_id)
    status_code, _, payload = _http_bytes(
        archive_url, "/api/v1/dump", accept="application/x-protobuf"
    )
    oracle.require(status_code == 200 and payload, "archive HTTP dump failed")
    _, serve_stderr = _interrupt(serve, 130)
    oracle.require(serve_stderr == b"", f"archive serve shutdown noise: {serve_stderr!r}")
    oracle.require(
        import_marker.read_text(encoding="ascii") == initial_imports,
        "archive diagnostic commands imported the Production",
    )

    preview = _canonical_cli_json(
        _run_cli(
            console,
            [
                "diagnostic",
                "cleanup",
                "--production",
                str(package),
                "--run",
                base_run_id,
                "--format",
                "json",
            ],
            environment=environment,
        )
    )
    oracle.require(
        preview["cleanup_preview_schema_version"] == 1
        and preview["policy_satisfied"] is True
        and preview["operation_error"] is None,
        "cleanup preview did not produce a valid exact-Run plan",
    )
    oracle.require(
        len(preview["runs"]) == 1
        and preview["runs"][0]["run_id"] == base_run_id
        and preview["runs"][0]["selected"] is True,
        "cleanup preview did not select the exact Run",
    )
    oracle.require(archive.exists(), "cleanup preview mutated the Run archive")
    applied = _canonical_cli_json(
        _run_cli(
            console,
            [
                "diagnostic",
                "cleanup",
                "--production",
                str(package),
                "--run",
                base_run_id,
                "--apply",
                "--format",
                "json",
            ],
            environment=environment,
        )
    )
    oracle.require(
        applied["cleanup_apply_schema_version"] == 1
        and applied["policy_satisfied"] is True
        and applied["operation_error"] is None,
        "cleanup apply did not satisfy policy",
    )
    oracle.require(
        len(applied["runs"]) == 1
        and applied["runs"][0]["run_id"] == base_run_id
        and applied["runs"][0]["disposition"] == "deleted",
        "cleanup apply did not report exact-Run deletion",
    )
    oracle.require(not archive.exists(), "cleanup apply left the Run archive")
    return events


def _run_case(
    console: Path,
    suite_root: Path,
    case: MatrixCase,
    repetition: int,
) -> str:
    case_root = suite_root / f"{case.identifier}-{repetition}"
    case_root.mkdir()
    package = _prepare_package(case_root)
    workspace = case_root / "workspace"
    workspace.mkdir()
    stage = case_root / "stage.json"
    trigger = case_root / "trigger"
    done = case_root / "done.json"
    terminal_trigger = case_root / "terminal-trigger"
    terminal_ready = case_root / "terminal-ready.json"
    sink = case_root / "sink.jsonl"
    agent_events = case_root / "agent.jsonl"
    import_marker = case_root / "imports"
    config = case_root / "config.json"
    config.write_text(
        json.dumps(
            {
                "provider": case.provider,
                "mcp_revision": case.mcp_revision,
                "capture_tool_payloads": case.capture_tool_payloads,
                "stage_path": str(stage),
                "trigger_path": str(trigger),
                "done_path": str(done),
                "terminal_trigger_path": str(terminal_trigger),
                "terminal_ready_path": str(terminal_ready),
                "sink_path": str(sink),
                "agent_events_path": str(agent_events),
                "workspace": str(workspace),
                "mock_path": str(MOCK),
            },
            sort_keys=True,
            separators=(",", ":"),
        ),
        encoding="utf-8",
    )
    environment = _environment(import_marker)
    children: list[subprocess.Popen[bytes]] = []
    production = subprocess.Popen(
        [
            str(console),
            "--production",
            str(package),
            "--diagnostic-port",
            "0",
            "--",
            str(config),
        ],
        cwd=case_root,
        env=environment,
        stdin=subprocess.DEVNULL,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
    )
    try:
        assert production.stderr is not None
        ready_line = _readline(production.stderr, production, TIMEOUT).rstrip(b"\n")
        oracle.require(ready_line.startswith(READY_PREFIX), f"bad Production ready: {ready_line!r}")
        locator = json.loads(ready_line.removeprefix(READY_PREFIX))
        _exact_keys(
            locator,
            {
                "locator_schema_version",
                "run_id",
                "local_url",
                "advertise_url",
                "archive_directory",
                "security_scope",
            },
            "ready locator",
        )
        run_id = locator["run_id"]
        base_url = locator["local_url"]
        archive = Path(locator["archive_directory"])
        oracle.require(locator["locator_schema_version"] == 1, "locator schema drift")
        oracle.require(base_url.startswith("http://127.0.0.1:"), "local URL is not loopback")
        oracle.require(locator["advertise_url"] is None, "unexpected advertised URL")
        oracle.require(locator["security_scope"] == "trusted_network", "security scope drift")
        oracle.require(archive.is_relative_to((package / ".troupe").resolve()), "archive escaped")
        _wait_file(stage, production, "first Cue stage")
        oracle.require(import_marker.read_text(encoding="ascii") == "1", "Production import count")
        _, stage_snapshot, stage_events = _active_cli_checks(
            console, package, base_url, run_id, environment
        )
        oracle.assert_dense_events(stage_events, run_id, prefix=True)
        stage_watermark = len(stage_events)
        stage_end = int(stage_snapshot["state"]["through_elapsed_ns"]) + 1
        stage_series = _http_json(
            base_url,
            "/api/v1/views?"
            + urlencode(
                {
                    "view_id": "queue_depth",
                    "viewport_start_ns": "0",
                    "viewport_end_ns": str(stage_end),
                }
            ),
        )
        stage_binding = stage_series["binding"]
        _asset_checks(base_url)

        live_sse = SseCapture(base_url, stage_watermark, stop_after_events=12)
        live_sse.start()
        follow = _start_follow(console, base_url, stage_watermark, environment)
        children.append(follow)
        time.sleep(0.05)
        oracle.require(follow.poll() is None, "CLI follow failed before live commit")
        trigger.write_text("continue\n", encoding="ascii")
        _wait_file(done, production, "second Scene completion")
        live_frames = live_sse.wait()
        live_events = _assert_sse(live_frames, run_id, stage_watermark)
        oracle.require(live_events, "live SSE observed no committed event")
        assert follow.stdout is not None
        _wait_pipe_readable(follow.stdout, follow, "CLI follow event")
        follow_stdout, follow_stderr = _interrupt(follow, 130)
        oracle.require(follow_stderr == b"", f"follow stderr: {follow_stderr!r}")
        followed = [json.loads(line) for line in follow_stdout.decode().splitlines()]
        oracle.require(followed, "CLI follow observed no committed event")
        oracle.require(
            [int(event["sequence"]) for event in followed]
            == sorted({int(event["sequence"]) for event in followed}),
            "CLI follow duplicated or reordered events",
        )

        _, _, active_events = _active_cli_checks(
            console, package, base_url, run_id, environment
        )
        oracle.assert_dense_events(active_events, run_id, prefix=True)
        finite_resume = _jsonl_stdout(
            _run_cli(
                console,
                [
                    "diagnostic",
                    "events",
                    "--url",
                    base_url,
                    "--after",
                    str(stage_watermark),
                    "--format",
                    "jsonl",
                ],
                environment=environment,
            )
        )
        oracle.require(
            finite_resume == active_events[stage_watermark:],
            "finite resume cursor did not recover the live suffix",
        )
        reconnect = SseCapture(
            base_url,
            len(active_events),
            stop_after_events=None,
            stop_after_ready=True,
        )
        reconnect.start()
        reconnect_frames = reconnect.wait()
        oracle.require(
            reconnect_frames[0]["data"]["resume_after"] == str(len(active_events)),
            "SSE reconnect did not preserve its resume cursor",
        )

        _view_checks(base_url, run_id, stage_binding)
        active_trace = case_root / "active.pftrace"
        dump_report = _dump(
            console, ("--url", base_url), active_trace, environment
        )
        _, active_summary = _decode_perfetto(active_trace)
        oracle.assert_perfetto_summary(
            active_summary, run_id, int(dump_report["exported_through"])
        )

        terminal_sse = SseCapture(base_url, len(active_events), stop_after_events=None)
        terminal_sse.start()
        terminal_follow = _start_follow(console, base_url, len(active_events), environment)
        children.append(terminal_follow)
        terminal_trigger.write_text("continue\n", encoding="ascii")
        _wait_file(terminal_ready, production, "terminal follow probe")
        assert terminal_follow.stdout is not None
        _wait_pipe_readable(terminal_follow.stdout, terminal_follow, "terminal CLI follow probe")
        production.send_signal(signal.SIGINT)
        stdout, stderr = production.communicate(timeout=TIMEOUT)
        oracle.require(production.returncode == 0, f"Production exit {production.returncode}")
        oracle.require(stdout == b"", f"Production stdout noise: {stdout!r}")
        oracle.require(stderr == b"", f"Production stderr noise: {stderr!r}")
        terminal_frames = terminal_sse.wait()
        terminal_events = _assert_sse(terminal_frames, run_id, len(active_events))
        oracle.require(
            terminal_frames[-1]["event"] == "stream_closed",
            "normal shutdown omitted stream_closed",
        )
        terminal_stdout, terminal_stderr = terminal_follow.communicate(timeout=TIMEOUT)
        oracle.require(
            terminal_follow.returncode == 0,
            "terminal CLI follow did not close normally: "
            f"exit={terminal_follow.returncode}, stdout={terminal_stdout!r}, "
            f"stderr={terminal_stderr!r}",
        )
        oracle.require(terminal_stderr == b"", f"terminal follow stderr: {terminal_stderr!r}")
        terminal_cli = [json.loads(line) for line in terminal_stdout.decode().splitlines()]
        oracle.require(terminal_cli == terminal_events, "CLI/SSE terminal suffix drift")

        archived_events = _archive_checks(
            console,
            package,
            archive,
            run_id,
            active_events,
            import_marker,
            environment,
            case_root,
            children,
        )
        sink_rows = oracle.read_jsonl(sink)
        oracle.assert_full_chain(
            archived_events,
            sink_rows,
            provider=case.provider,
            capture_tool_payloads=case.capture_tool_payloads,
            usage_availability=case.usage_availability,
        )
        agent_rows = oracle.read_jsonl(agent_events)
        oracle.require(
            sum(row["event"] == "session_new" for row in agent_rows) == 1,
            "Actor did not preserve one persistent agent session",
        )
        oracle.require(
            [row["turn"] for row in agent_rows if row["event"] == "turn_completed"]
            == [1, 2, 3],
            "mock ACP turn sequence drifted",
        )
        oracle.require(not (FIXTURE / ".troupe").exists(), "checked-in fixture was mutated")
        return run_id
    finally:
        if production.poll() is None:
            production.send_signal(signal.SIGINT)
            try:
                production.communicate(timeout=5)
            except subprocess.TimeoutExpired:
                production.kill()
                production.communicate()
        for child in children:
            _stop_child(child)


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--matrix", type=Path, required=True)
    parser.add_argument("--repeat", type=int, default=1)
    parser.add_argument("--case", dest="case_id")
    parser.add_argument("--keep-temp", action="store_true")
    args = parser.parse_args()
    oracle.require(args.repeat > 0, "--repeat must be positive")
    cases = _load_matrix(args.matrix.resolve())
    if args.case_id is not None:
        cases = [case for case in cases if case.identifier == args.case_id]
        oracle.require(cases, f"unknown matrix case {args.case_id!r}")

    parent = Path(os.environ.get("TROUPE_GATE_TMP", tempfile.gettempdir())).resolve()
    parent.mkdir(parents=True, exist_ok=True)
    suite_root = Path(tempfile.mkdtemp(prefix="troupe-diagnostics-e2e-", dir=parent))
    console = _console()
    run_ids: list[str] = []
    try:
        for repetition in range(1, args.repeat + 1):
            for case in cases:
                print(f"V02 {case.identifier} repetition {repetition}/{args.repeat}", flush=True)
                run_ids.append(_run_case(console, suite_root, case, repetition))
        oracle.require(len(run_ids) == len(set(run_ids)), "Run identities leaked across cases")
        oracle.require(not (FIXTURE / ".troupe").exists(), "fixture contains .troupe")
        print(f"V02 passed {len(run_ids)} isolated Production runs", flush=True)
        return 0
    except BaseException:
        print(f"V02 retained failing workspace: {suite_root}", file=sys.stderr, flush=True)
        raise
    finally:
        if not args.keep_temp and sys.exc_info()[0] is None:
            shutil.rmtree(suite_root)


if __name__ == "__main__":
    raise SystemExit(main())
