#!/usr/bin/env python3
"""Execute one registered diagnostics fault bundle in an isolated process."""

from __future__ import annotations

import argparse
import json
import os
import signal
import subprocess
import sys
import sysconfig
import time
from pathlib import Path
from typing import Sequence

import fault_adapter
import oracle


ROOT = Path(__file__).resolve().parents[3]


def _parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser()
    parser.add_argument("--matrix", type=Path, required=True)
    parser.add_argument("--case", required=True)
    parser.add_argument("--root", type=Path, required=True)
    return parser


def _isolated_environment(root: Path) -> dict[str, str]:
    environment = dict(os.environ)
    temporary = root / "tmp"
    cache = root / "cache"
    temporary.mkdir()
    cache.mkdir()
    environment.update(
        {
            "TMP": str(temporary),
            "TMPDIR": str(temporary),
            "XDG_CACHE_HOME": str(cache),
            "PYTHONDONTWRITEBYTECODE": "1",
            "PYTEST_ADDOPTS": "-p no:cacheprovider",
            "TROUPE_DIAGNOSTIC_PORT": "0",
            "TROUPE_FAILURE_CASE_ROOT": str(root),
        }
    )
    libdir = sysconfig.get_config_var("LIBDIR")
    if libdir:
        inherited = environment.get("LD_LIBRARY_PATH")
        environment["LD_LIBRARY_PATH"] = (
            f"{libdir}{os.pathsep}{inherited}" if inherited else str(libdir)
        )
    return environment


def _command_runner(environment: dict[str, str]):
    def run(spec: fault_adapter.CommandSpec) -> str:
        started = time.monotonic()
        process = subprocess.Popen(
            spec.argv,
            cwd=ROOT,
            env=environment,
            stdin=subprocess.DEVNULL,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            start_new_session=True,
        )
        try:
            stdout, stderr = process.communicate(timeout=spec.timeout_seconds)
        except subprocess.TimeoutExpired as error:
            try:
                os.killpg(process.pid, signal.SIGKILL)
            except ProcessLookupError:
                pass
            stdout, stderr = process.communicate()
            raise fault_adapter.FaultAdapterError(
                f"command timed out after {spec.timeout_seconds:.0f}s: {spec.argv!r}"
            ) from error
        if process.returncode != 0:
            output = (stdout + b"\n" + stderr).decode(errors="replace")[-8000:]
            raise fault_adapter.FaultAdapterError(
                f"command exited {process.returncode}: {spec.argv!r}\n{output}"
            )
        elapsed_ms = int((time.monotonic() - started) * 1000)
        executable = Path(spec.argv[0]).name
        target = next(
            (spec.argv[index + 1] for index, item in enumerate(spec.argv[:-1]) if item == "--test"),
            None,
        )
        return f"{executable}:{target or 'lib'}:{elapsed_ms}ms"

    return run


def main(argv: Sequence[str] | None = None) -> int:
    arguments = _parser().parse_args(argv)
    try:
        matrix = arguments.matrix.resolve(strict=True)
        root = arguments.root.resolve(strict=True)
        oracle.require(root.is_dir(), "child root must be a directory")
        oracle.require(not any(root.iterdir()), "child root must start empty")
        _, cases = oracle.load_matrix(matrix)
        by_id = {case.identifier: case for case in cases}
        case = by_id.get(arguments.case)
        oracle.require(case is not None, f"unknown matrix case: {arguments.case}")
        fault_adapter.assert_adapter_inventory(oracle.adapter_checks(cases))
        environment = _isolated_environment(root)
        started = time.monotonic()
        result = fault_adapter.run_adapter(
            case.adapter,
            root,
            environment,
            _command_runner(environment),
        )
        payload = {
            "result_schema_version": 1,
            "case_id": case.identifier,
            "adapter": case.adapter,
            "pid": os.getpid(),
            "requested_port": 0,
            "commands": list(result.commands),
            "assertions": result.assertions,
            "duration_ms": int((time.monotonic() - started) * 1000),
            "status": "passed",
        }
        oracle.validate_child_result(payload, case)
        print(json.dumps(payload, sort_keys=True, separators=(",", ":")))
    except (
        OSError,
        KeyError,
        TypeError,
        ValueError,
        subprocess.SubprocessError,
        oracle.OracleError,
        fault_adapter.FaultAdapterError,
    ) as error:
        print(f"V06 child {arguments.case}: {error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
