#!/usr/bin/env python3
"""Run the V06 diagnostics failure matrix in isolated child processes."""

from __future__ import annotations

import argparse
import concurrent.futures
import hashlib
import json
import os
import random
import shutil
import signal
import subprocess
import sys
import tempfile
import time
from dataclasses import dataclass
from pathlib import Path
from typing import Sequence

import fault_adapter
import oracle


ROOT = Path(__file__).resolve().parents[3]
CHILD = Path(__file__).with_name("child_harness.py")
CHILD_TIMEOUT = 900.0


@dataclass(frozen=True, slots=True)
class ChildOutcome:
    case_id: str
    result: dict[str, object] | None
    error: str | None


def _parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser()
    parser.add_argument("--matrix", type=Path, required=True)
    parser.add_argument("--random-order", action="store_true")
    parser.add_argument("--parallel-runs", type=int, default=1)
    return parser


def _suite_parent() -> Path | None:
    value = os.environ.get("TROUPE_GATE_TMP")
    if value is None:
        return None
    parent = Path(value).resolve(strict=True)
    oracle.require(parent.is_dir(), "TROUPE_GATE_TMP must be a directory")
    return parent


def _run_child(
    matrix: Path,
    suite_root: Path,
    case: oracle.MatrixCase,
) -> ChildOutcome:
    case_root = suite_root / case.identifier
    case_root.mkdir(mode=0o700)
    environment = dict(os.environ)
    environment["PYTHONDONTWRITEBYTECODE"] = "1"
    process = subprocess.Popen(
        [
            sys.executable,
            "-B",
            str(CHILD),
            "--matrix",
            str(matrix),
            "--case",
            case.identifier,
            "--root",
            str(case_root),
        ],
        cwd=ROOT,
        env=environment,
        stdin=subprocess.DEVNULL,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        start_new_session=True,
    )
    try:
        stdout, stderr = process.communicate(timeout=CHILD_TIMEOUT)
    except subprocess.TimeoutExpired:
        try:
            os.killpg(process.pid, signal.SIGKILL)
        except ProcessLookupError:
            pass
        process.communicate()
        return ChildOutcome(case.identifier, None, "child exceeded the total timeout")
    if process.returncode != 0:
        detail = (stdout + b"\n" + stderr).decode(errors="replace")[-10000:]
        return ChildOutcome(case.identifier, None, detail.strip())
    lines = stdout.decode().splitlines()
    if len(lines) != 1 or stderr:
        return ChildOutcome(
            case.identifier,
            None,
            f"non-canonical child output: stdout={stdout!r}, stderr={stderr!r}",
        )
    try:
        value = json.loads(lines[0])
        validated = oracle.validate_child_result(value, case)
    except (json.JSONDecodeError, oracle.OracleError) as error:
        return ChildOutcome(case.identifier, None, str(error))
    return ChildOutcome(case.identifier, validated, None)


def main(argv: Sequence[str] | None = None) -> int:
    arguments = _parser().parse_args(argv)
    suite_root: Path | None = None
    started = time.monotonic()
    try:
        oracle.require(1 <= arguments.parallel_runs <= 8, "parallel-runs must be in 1..8")
        matrix = arguments.matrix
        if not matrix.is_absolute():
            matrix = ROOT / matrix
        matrix = matrix.resolve(strict=True)
        _, loaded_cases = oracle.load_matrix(matrix)
        fault_adapter.assert_adapter_inventory(oracle.adapter_checks(loaded_cases))
        cases = list(loaded_cases)
        seed = int(hashlib.sha256(matrix.read_bytes()).hexdigest()[:16], 16)
        if arguments.random_order:
            random.Random(seed).shuffle(cases)

        suite_root = Path(
            tempfile.mkdtemp(prefix="troupe-v06-", dir=_suite_parent())
        ).resolve(strict=True)
        outcomes: list[ChildOutcome] = []
        with concurrent.futures.ThreadPoolExecutor(
            max_workers=arguments.parallel_runs,
            thread_name_prefix="v06-child",
        ) as executor:
            future_cases = {
                executor.submit(_run_child, matrix, suite_root, case): case
                for case in cases
            }
            for future in concurrent.futures.as_completed(future_cases):
                outcome = future.result()
                outcomes.append(outcome)
                if outcome.error is None:
                    duration = outcome.result["duration_ms"] if outcome.result else "?"
                    print(f"V06 PASS {outcome.case_id} ({duration} ms)", file=sys.stderr)
                else:
                    print(f"V06 FAIL {outcome.case_id}", file=sys.stderr)

        failures = [outcome for outcome in outcomes if outcome.error is not None]
        if failures:
            for failure in sorted(failures, key=lambda item: item.case_id):
                print(f"\n[{failure.case_id}]\n{failure.error}", file=sys.stderr)
            return 1
        results = [outcome.result for outcome in outcomes if outcome.result is not None]
        pids = {result["pid"] for result in results}
        oracle.require(len(pids) == len(cases), "matrix cases did not use distinct child processes")
        summary = {
            "result_schema_version": 1,
            "cases": len(cases),
            "parallel_runs": arguments.parallel_runs,
            "randomized": arguments.random_order,
            "seed": str(seed),
            "assertions": sum(int(result["assertions"]) for result in results),
            "duration_ms": int((time.monotonic() - started) * 1000),
            "status": "passed",
        }
        print(json.dumps(summary, sort_keys=True, separators=(",", ":")))
    except (
        OSError,
        KeyError,
        TypeError,
        ValueError,
        subprocess.SubprocessError,
        oracle.OracleError,
        fault_adapter.FaultAdapterError,
    ) as error:
        print(f"V06 failure matrix: {error}", file=sys.stderr)
        return 1
    finally:
        if suite_root is not None and suite_root.exists():
            shutil.rmtree(suite_root)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
