#!/usr/bin/env python3
"""Exercise packaged diagnostics using only an installed wheel and the stdlib."""

from __future__ import annotations

import argparse
import hashlib
import http.client
import json
import os
import re
import selectors
import shutil
import signal
import subprocess
import sys
import time
from pathlib import Path
from typing import IO, Any, cast
from urllib.parse import urljoin, urlsplit


READY_PREFIX = b"troupe: diagnostic ready "
ARCHIVE_READY_PREFIX = b"troupe: diagnostic archive ready "
DUMP_PREFIX = b"troupe: diagnostic dump "
FORBIDDEN_TOOLS = (
    "node",
    "nodejs",
    "npm",
    "npx",
    "protoc",
    "perfetto",
    "trace_processor_shell",
)
TIMEOUT = 30.0


class SmokeError(RuntimeError):
    pass


def require(condition: bool, message: str) -> None:
    if not condition:
        raise SmokeError(message)


def sha256_file(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as stream:
        while chunk := stream.read(1024 * 1024):
            digest.update(chunk)
    return digest.hexdigest()


def canonical_json(value: object) -> bytes:
    return json.dumps(value, separators=(",", ":")).encode("utf-8")


def read_ready_line(
    pipe: IO[bytes],
    process: subprocess.Popen[bytes],
    prefix: bytes,
) -> dict[str, Any]:
    selector = selectors.DefaultSelector()
    selector.register(pipe, selectors.EVENT_READ)
    try:
        deadline = time.monotonic() + TIMEOUT
        while True:
            remaining = deadline - time.monotonic()
            require(remaining > 0, "timed out waiting for diagnostic readiness")
            if not selector.select(min(remaining, 0.1)):
                require(
                    process.poll() is None, "diagnostic process exited before readiness"
                )
                continue
            line = pipe.readline().rstrip(b"\n")
            require(line.startswith(prefix), f"unexpected readiness line: {line!r}")
            value: object = json.loads(line.removeprefix(prefix))
            require(
                isinstance(value, dict), "diagnostic readiness locator is not an object"
            )
            return cast(dict[str, Any], value)
    finally:
        selector.close()


def wait_for_file(path: Path, process: subprocess.Popen[bytes], context: str) -> None:
    deadline = time.monotonic() + TIMEOUT
    while not path.is_file():
        require(process.poll() is None, f"Production exited before {context}")
        require(time.monotonic() < deadline, f"timed out waiting for {context}")
        time.sleep(0.01)


def request(url: str, *, accept: str) -> tuple[int, dict[str, str], bytes]:
    parsed = urlsplit(url)
    require(parsed.scheme == "http", "diagnostic URL is not HTTP")
    host = parsed.hostname
    port = parsed.port
    if host is None or port is None:
        raise SmokeError("diagnostic URL is incomplete")
    connection = http.client.HTTPConnection(host, port, timeout=TIMEOUT)
    try:
        path = parsed.path or "/"
        if parsed.query:
            path = f"{path}?{parsed.query}"
        connection.request(
            "GET",
            path,
            headers={"Accept": accept, "Connection": "close"},
        )
        response = connection.getresponse()
        headers = {name.lower(): value for name, value in response.getheaders()}
        return response.status, headers, response.read()
    finally:
        connection.close()


def endpoint(base_url: str, suffix: str) -> str:
    parsed = urlsplit(base_url)
    prefix = parsed.path.rstrip("/")
    return parsed._replace(path=f"{prefix}{suffix}", query="", fragment="").geturl()


def json_endpoint(base_url: str, suffix: str) -> dict[str, Any]:
    status, headers, payload = request(
        endpoint(base_url, suffix), accept="application/json"
    )
    require(status == 200, f"GET {suffix} returned {status}")
    require(
        headers.get("content-type", "").startswith("application/json"),
        "JSON MIME drifted",
    )
    value: object = json.loads(payload)
    require(isinstance(value, dict), f"GET {suffix} did not return an object")
    return cast(dict[str, Any], value)


def asset_smoke(base_url: str) -> dict[str, object]:
    status, headers, payload = request(base_url, accept="text/html")
    require(status == 200 and bool(payload), "diagnostic UI root is unavailable")
    require(
        headers.get("content-type", "").startswith("text/html"), "UI root is not HTML"
    )
    html = payload.decode("utf-8")
    assets = sorted(set(re.findall(r'(?:src|href)="([^\"]+/assets/[^\"]+)"', html)))
    require(bool(assets), "diagnostic UI has no embedded asset references")
    hashes: list[dict[str, object]] = []
    for relative in assets:
        asset_url = urljoin(base_url.rstrip("/") + "/", relative)
        asset_status, asset_headers, asset_payload = request(asset_url, accept="*/*")
        require(
            asset_status == 200 and bool(asset_payload),
            f"diagnostic asset is unavailable: {relative}",
        )
        require(
            "content-type" in asset_headers, f"diagnostic asset has no MIME: {relative}"
        )
        hashes.append(
            {
                "path": urlsplit(asset_url).path,
                "bytes": len(asset_payload),
                "sha256": hashlib.sha256(asset_payload).hexdigest(),
            }
        )
    return {
        "html_bytes": len(payload),
        "html_sha256": hashlib.sha256(payload).hexdigest(),
        "assets": hashes,
    }


def run_cli(
    console: Path, arguments: list[str], workspace: Path
) -> subprocess.CompletedProcess[bytes]:
    try:
        completed = subprocess.run(
            [str(console), *arguments],
            cwd=workspace,
            env=os.environ,
            stdin=subprocess.DEVNULL,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            timeout=TIMEOUT,
            check=False,
        )
    except (OSError, subprocess.TimeoutExpired) as error:
        raise SmokeError(f"could not run packaged CLI: {error}") from error
    require(
        completed.returncode == 0,
        f"packaged CLI exited {completed.returncode}: {completed.stderr!r}",
    )
    return completed


def cli_json(console: Path, arguments: list[str], workspace: Path) -> dict[str, Any]:
    completed = run_cli(console, arguments, workspace)
    require(completed.stderr == b"", f"CLI JSON emitted stderr: {completed.stderr!r}")
    value: object = json.loads(completed.stdout)
    require(isinstance(value, dict), "CLI JSON result is not an object")
    require(
        completed.stdout == canonical_json(value) + b"\n", "CLI JSON is not canonical"
    )
    return cast(dict[str, Any], value)


def stop_process(process: subprocess.Popen[bytes]) -> None:
    if process.poll() is not None:
        return
    process.send_signal(signal.SIGINT)
    try:
        process.communicate(timeout=5)
    except subprocess.TimeoutExpired:
        process.kill()
        process.communicate()


def write_production(package: Path) -> tuple[Path, Path]:
    package.mkdir()
    (package / "__init__.py").write_text("", encoding="ascii")
    stage = package.parent / "scene-entered"
    import_marker = package.parent / "production-imports"
    source = """from __future__ import annotations

import asyncio
import os
from pathlib import Path

import troupe


with Path(os.environ["TROUPE_WHEEL_IMPORT_MARKER"]).open("a", encoding="ascii") as stream:
    stream.write("imported\\n")


class Production(troupe.Production):
    def __init__(self, args: list[str]) -> None:
        self.stage = Path(args[0])

    async def scene(self) -> None:
        self.stage.write_text("entered\\n", encoding="ascii")
        await asyncio.Event().wait()
"""
    (package / "production.py").write_text(source, encoding="ascii")
    return stage, import_marker


def exercise(workspace: Path, modes: tuple[str, ...]) -> dict[str, object]:
    for tool in FORBIDDEN_TOOLS:
        require(
            shutil.which(tool) is None,
            f"forbidden tool is available during wheel smoke: {tool}",
        )

    console_value = shutil.which("troupe")
    if console_value is None:
        raise SmokeError("installed troupe console is unavailable")
    console = Path(console_value).resolve(strict=True)

    import troupe  # type: ignore[import-not-found]
    import troupe._runtime as runtime  # type: ignore[import-not-found]

    environment_root = Path(sys.prefix).resolve(strict=True)
    troupe_file = Path(troupe.__file__).resolve(strict=True)
    native_file = Path(runtime.__file__).resolve(strict=True)
    for path, label in ((troupe_file, "wrapper"), (native_file, "native module")):
        try:
            path.relative_to(environment_root)
        except ValueError as error:
            raise SmokeError(
                f"installed {label} was imported outside the child environment"
            ) from error

    package = workspace / "diagnostic_production"
    stage, import_marker = write_production(package)
    environment = dict(os.environ)
    environment["TROUPE_WHEEL_IMPORT_MARKER"] = str(import_marker)
    production = subprocess.Popen(
        [str(console), "--production", str(package), "--", str(stage)],
        cwd=workspace,
        env=environment,
        stdin=subprocess.DEVNULL,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
    )
    archive_server: subprocess.Popen[bytes] | None = None
    try:
        if production.stderr is None:
            raise SmokeError("Production stderr pipe is unavailable")
        locator = read_ready_line(production.stderr, production, READY_PREFIX)
        required_locator = {
            "locator_schema_version",
            "run_id",
            "local_url",
            "advertise_url",
            "archive_directory",
            "security_scope",
        }
        require(set(locator) == required_locator, "active locator fields drifted")
        require(
            locator["locator_schema_version"] == 1, "active locator version drifted"
        )
        require(
            locator["advertise_url"] is None,
            "wheel smoke unexpectedly advertised a URL",
        )
        require(
            locator["security_scope"] == "trusted_network", "security scope drifted"
        )
        base_url = str(locator["local_url"])
        run_id = str(locator["run_id"])
        archive = Path(str(locator["archive_directory"])).resolve()
        require(
            base_url.startswith("http://127.0.0.1:"),
            "local diagnostic URL is not loopback",
        )
        require(
            archive.is_relative_to((package / ".troupe").resolve()),
            "archive escaped Production",
        )
        wait_for_file(stage, production, "Scene entry")
        require(
            import_marker.read_text(encoding="ascii") == "imported\n",
            "Production import count drifted",
        )

        active_status = cli_json(
            console,
            ["diagnostic", "status", "--production", str(package), "--format", "json"],
            workspace,
        )
        require(active_status["source"] == "active", "active status source drifted")
        require(str(active_status["run_id"]) == run_id, "active status Run drifted")
        require(
            json_endpoint(base_url, "/api/v1/status")["source"] == "active",
            "active HTTP status drifted",
        )
        active_ui = asset_smoke(base_url)

        production.send_signal(signal.SIGINT)
        stdout, stderr = production.communicate(timeout=TIMEOUT)
        require(
            production.returncode == 0, f"Production exited {production.returncode}"
        )
        require(stdout == b"", f"Production emitted stdout: {stdout!r}")
        require(stderr == b"", f"Production emitted shutdown stderr: {stderr!r}")
        require(archive.is_dir(), "Production archive was not retained")

        archive_status = cli_json(
            console,
            ["diagnostic", "status", "--archive", str(archive), "--format", "json"],
            workspace,
        )
        require(archive_status["source"] == "archive", "archive status source drifted")
        require(str(archive_status["run_id"]) == run_id, "archive status Run drifted")

        trace = workspace / "archive.pftrace"
        dump = run_cli(
            console,
            ["diagnostic", "dump", "--archive", str(archive), "--output", str(trace)],
            workspace,
        )
        require(dump.stdout == b"", "diagnostic dump emitted stdout")
        line = dump.stderr.strip()
        require(
            line.startswith(DUMP_PREFIX), f"diagnostic dump report drifted: {line!r}"
        )
        dump_value: object = json.loads(line.removeprefix(DUMP_PREFIX))
        require(isinstance(dump_value, dict), "diagnostic dump report is not an object")
        dump_report = cast(dict[str, Any], dump_value)
        require(
            dump_report["publication"] == "published",
            "diagnostic dump was not published",
        )
        require(
            trace.is_file() and trace.stat().st_size > 0, "diagnostic dump is empty"
        )

        archive_server = subprocess.Popen(
            [
                str(console),
                "diagnostic",
                "serve",
                "--archive",
                str(archive),
                "--port",
                "0",
            ],
            cwd=workspace,
            env=environment,
            stdin=subprocess.DEVNULL,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
        )
        if archive_server.stderr is None:
            raise SmokeError("archive server stderr pipe is unavailable")
        archive_locator = read_ready_line(
            archive_server.stderr,
            archive_server,
            ARCHIVE_READY_PREFIX,
        )
        archive_url = str(archive_locator["local_url"])
        require(str(archive_locator["run_id"]) == run_id, "archive serve Run drifted")
        require(
            archive_url.startswith("http://127.0.0.1:"),
            "archive server is not loopback",
        )
        require(
            json_endpoint(archive_url, "/api/v1/status")["source"] == "archive",
            "archive HTTP status drifted",
        )
        archive_ui = asset_smoke(archive_url)
        archive_server.send_signal(signal.SIGINT)
        serve_stdout, serve_stderr = archive_server.communicate(timeout=TIMEOUT)
        require(
            archive_server.returncode == 130,
            f"archive serve exited {archive_server.returncode}",
        )
        require(
            serve_stdout == b"" and serve_stderr == b"",
            "archive serve shutdown emitted output",
        )

        require(
            import_marker.read_text(encoding="ascii") == "imported\n",
            "archive command imported Production",
        )
        return {
            "modes": list(modes),
            "run_id": run_id,
            "installed": {
                "environment": str(environment_root),
                "troupe_file": str(troupe_file),
                "native_file": str(native_file),
                "native_bytes": native_file.stat().st_size,
                "native_sha256": sha256_file(native_file),
            },
            "active": {"status": "passed", "ui": active_ui},
            "archive": {
                "status": "passed",
                "ui": archive_ui,
                "trace_bytes": trace.stat().st_size,
                "trace_sha256": sha256_file(trace),
            },
            "forbidden_tools": list(FORBIDDEN_TOOLS),
            "production_imports": 1,
        }
    finally:
        stop_process(production)
        if archive_server is not None:
            stop_process(archive_server)


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--workspace", type=Path, required=True)
    parser.add_argument("--smoke", required=True)
    arguments = parser.parse_args()
    modes = tuple(arguments.smoke.split(","))
    require(
        modes == ("active", "archive"),
        "wheel smoke modes must be exactly active,archive",
    )
    workspace = arguments.workspace.resolve(strict=True)
    require(workspace.is_dir(), "wheel smoke workspace must be a directory")
    try:
        result = exercise(workspace, modes)
    except (SmokeError, OSError, ValueError, KeyError, json.JSONDecodeError) as error:
        print(f"troupe diagnostics wheel smoke failed: {error}", file=sys.stderr)
        return 1
    print(canonical_json(result).decode("utf-8"))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
