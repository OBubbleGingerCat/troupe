#!/usr/bin/env bash
set -euo pipefail

repository_root="$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd -P)"
python_executable="$(command -v python3 || command -v python)"
export PYTHONDONTWRITEBYTECODE=1
exec "$python_executable" -B - "$repository_root" "$@" <<'PY'
from __future__ import annotations

import argparse
import hashlib
import importlib.util
import json
import os
import shutil
import signal
import stat
import subprocess
import sys
import tempfile
import time
from pathlib import Path
from typing import Any, Mapping, Sequence


ROOT = Path(sys.argv[1]).resolve(strict=True)
ARGUMENTS = sys.argv[2:]
PUBLISHER_PATH = ROOT / "scripts/publish_diagnostics_acceptance.py"
PUBLISHER_SPEC = importlib.util.spec_from_file_location(
    "diagnostics_acceptance_publisher_for_final", PUBLISHER_PATH
)
if PUBLISHER_SPEC is None or PUBLISHER_SPEC.loader is None:
    raise SystemExit("final diagnostics runner: could not load acceptance publisher")
publisher = importlib.util.module_from_spec(PUBLISHER_SPEC)
sys.modules[PUBLISHER_SPEC.name] = publisher
PUBLISHER_SPEC.loader.exec_module(publisher)

FINAL_REPORT_NAME = "V03-final-evidence.json"
CACHE_IDENTITIES = {
    "npm": ".troupe-npm-cache.json",
    "playwright": ".troupe-playwright-cache.json",
    "perfetto": ".troupe-perfetto-cache.json",
}
TIMEOUTS = (21600, 7200, 3600, 3600, 1800, 1800, 1800, 1800, 600, 600, 600)


class FinalRunnerError(RuntimeError):
    pass


def fail(message: str) -> None:
    raise FinalRunnerError(message)


def sha256_file(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as stream:
        while chunk := stream.read(1024 * 1024):
            digest.update(chunk)
    return digest.hexdigest()


def sha256_bytes(value: bytes) -> str:
    return hashlib.sha256(value).hexdigest()


def exact_directory(path: Path, label: str) -> Path:
    try:
        return publisher._exact_directory(path, label)
    except publisher.AcceptanceError as error:
        fail(str(error))


def exact_cache(path: Path, label: str, identity_name: str) -> tuple[Path, str]:
    resolved = exact_directory(path, label)
    identity = resolved / identity_name
    try:
        metadata = identity.lstat()
        actual = identity.resolve(strict=True)
    except OSError as error:
        fail(f"{label} identity is unavailable: {error}")
    if (
        not stat.S_ISREG(metadata.st_mode)
        or stat.S_ISLNK(metadata.st_mode)
        or actual != identity
        or metadata.st_mode & 0o222
    ):
        fail(f"{label} identity must be an exact read-only regular file")
    return resolved, sha256_file(identity)


def command_output_hash(path: Path) -> str:
    return sha256_file(path)


def stream_file(path: Path, destination: Any) -> None:
    with path.open("r", encoding="utf-8", errors="replace") as stream:
        shutil.copyfileobj(stream, destination)
    destination.flush()


def terminate(process: subprocess.Popen[bytes]) -> None:
    if process.poll() is not None:
        return
    try:
        os.killpg(process.pid, signal.SIGTERM)
    except ProcessLookupError:
        return
    try:
        process.wait(timeout=10)
    except subprocess.TimeoutExpired:
        try:
            os.killpg(process.pid, signal.SIGKILL)
        except ProcessLookupError:
            pass
        process.wait()


def execute(
    actual: Sequence[str],
    *,
    stdout_path: Path,
    stderr_path: Path,
    environment: Mapping[str, str],
    timeout_seconds: int,
) -> int:
    with stdout_path.open("wb") as stdout, stderr_path.open("wb") as stderr:
        process = subprocess.Popen(
            list(actual),
            cwd=ROOT,
            env=dict(environment),
            stdout=stdout,
            stderr=stderr,
            start_new_session=True,
        )
        try:
            return process.wait(timeout=timeout_seconds)
        except subprocess.TimeoutExpired:
            terminate(process)
            stderr.write(
                f"final diagnostics runner: child timed out after {timeout_seconds}s\n".encode()
            )
            return 124
        except KeyboardInterrupt:
            terminate(process)
            return 130


def append_error(path: Path, message: str) -> None:
    with path.open("ab") as stream:
        stream.write(f"final diagnostics runner: {message}\n".encode())


def process_ancestors() -> set[int]:
    result = {os.getpid()}
    current = os.getpid()
    while current > 1:
        try:
            fields = Path(f"/proc/{current}/stat").read_text(encoding="ascii").split()
            parent = int(fields[3])
        except (OSError, ValueError, IndexError):
            break
        if parent in result:
            break
        result.add(parent)
        current = parent
    return result


def zero_audit(initial_status: bytes, runner_tmp: Path) -> None:
    status = subprocess.run(
        ["git", "status", "--porcelain=v1", "--untracked-files=all", "-z"],
        cwd=ROOT,
        check=True,
        capture_output=True,
    ).stdout
    if status != initial_status:
        fail("tracked or untracked checkout state changed during the final suite")

    worktrees = subprocess.run(
        ["git", "worktree", "list", "--porcelain"],
        cwd=ROOT,
        check=True,
        capture_output=True,
        text=True,
    ).stdout
    paths = [
        Path(line.removeprefix("worktree ")).resolve(strict=True)
        for line in worktrees.splitlines()
        if line.startswith("worktree ")
    ]
    if paths != [ROOT]:
        fail("diagnostic implementation worktrees remain")
    branches = subprocess.run(
        ["git", "for-each-ref", "--format=%(refname)", "refs/heads/diag/"],
        cwd=ROOT,
        check=True,
        capture_output=True,
        text=True,
    ).stdout.splitlines()
    if branches:
        fail("diagnostic implementation branches remain")

    for instances in ROOT.rglob("instances"):
        if instances.parent.name == "diagnostics" and instances.is_dir():
            if any(path.is_file() or path.is_symlink() for path in instances.iterdir()):
                fail(f"published diagnostic registry entries remain: {instances}")

    ancestors = process_ancestors()
    repository_bytes = os.fsencode(str(ROOT))
    for entry in Path("/proc").iterdir():
        if not entry.name.isdigit() or int(entry.name) in ancestors:
            continue
        try:
            command = (entry / "cmdline").read_bytes()
        except OSError:
            continue
        try:
            cwd = (entry / "cwd").resolve(strict=True)
        except OSError:
            cwd = None
        lowered = command.lower()
        references_repository = repository_bytes in command or (
            cwd is not None and (cwd == ROOT or ROOT in cwd.parents)
        )
        if references_repository and (b"troupe" in lowered or b"diagnostic" in lowered):
            fail(f"diagnostic child process remains: pid {entry.name}")

    for candidate in runner_tmp.parent.glob("troupe-diagnostics-final.*"):
        if candidate != runner_tmp and (candidate / ".troupe-final-owned").is_file():
            fail(f"owned final-runner temporary directory remains: {candidate}")


def parse(arguments: Sequence[str]) -> argparse.Namespace:
    parser = argparse.ArgumentParser(description="Run the complete diagnostics release closure")
    mode = parser.add_mutually_exclusive_group(required=True)
    mode.add_argument("--all", action="store_true")
    mode.add_argument("--verify-dispatch", action="store_true")
    parser.add_argument("--npm-cache", type=Path)
    parser.add_argument("--perfetto-cache", type=Path)
    parser.add_argument("--browser-cache", type=Path)
    parser.add_argument("--evidence-root", required=True, type=Path)
    parser.add_argument("--acceptance-path", required=True, type=Path)
    parser.add_argument("--attempt-id", required=True)
    parser.add_argument("--integration-sha", required=True)
    namespace = parser.parse_args(arguments)
    if namespace.all and not all(
        (namespace.npm_cache, namespace.perfetto_cache, namespace.browser_cache)
    ):
        parser.error("--all requires --npm-cache, --perfetto-cache, and --browser-cache")
    if namespace.verify_dispatch and any(
        (namespace.npm_cache, namespace.perfetto_cache, namespace.browser_cache)
    ):
        parser.error("--verify-dispatch does not accept real cache paths")
    return namespace


def prepare_paths(namespace: argparse.Namespace) -> tuple[Path, Path, Path]:
    try:
        attempt_id = publisher._canonical_attempt_id(namespace.attempt_id)
    except publisher.AcceptanceError as error:
        fail(str(error))
    evidence = namespace.evidence_root
    acceptance = namespace.acceptance_path
    if namespace.verify_dispatch and not evidence.exists() and not evidence.is_symlink():
        evidence.mkdir(parents=True, mode=0o700)
    attempt = exact_directory(evidence, "final evidence root")
    if attempt.name != attempt_id or attempt.parent.name != "attempts":
        fail("final evidence root must be the exact attempts/<attempt-id> directory")
    attempts = exact_directory(attempt.parent, "attempts directory")
    base = exact_directory(attempts.parent, "evidence base")
    if acceptance != base / "accepted.json":
        fail("acceptance path must be the exact accepted.json child of evidence base")
    if acceptance.exists() or acceptance.is_symlink():
        fail("acceptance path must be create-new")
    if any(attempt.iterdir()):
        fail("final evidence root must be fresh and empty")
    if publisher.COMMIT_RE.fullmatch(namespace.integration_sha) is None:
        fail("integration SHA must be 40 lowercase hexadecimal characters")
    return base, attempt, acceptance


def child_manifest(
    evidence: Path, product_base_sha: str
) -> list[tuple[str, list[str]]]:
    return [
        (
            "linux-release",
            [
                "scripts/test_linux_release.sh",
                "all",
                "--diagnostics-evidence-root",
                str(evidence),
            ],
        ),
        ("diagnostics-e2e", ["scripts/test_diagnostics_e2e.sh", "--all"]),
        ("O00", ["scripts/run_diagnostic_node_gate.sh", "O00"]),
        ("O01", ["scripts/run_diagnostic_node_gate.sh", "O01"]),
        ("O02", ["scripts/run_diagnostic_bootstrap_gate.sh", "O02"]),
        ("O03", ["scripts/run_diagnostic_node_gate.sh", "O03"]),
        ("O04", ["scripts/run_diagnostic_bootstrap_gate.sh", "O04"]),
        ("V11", ["scripts/run_diagnostic_bootstrap_gate.sh", "V11"]),
        (
            "plan",
            [
                "python",
                "docs/plan/verify_production_diagnostics_plan.py",
                "--self-test",
                "docs/plan/production-diagnostics-implementation-plan.md",
            ],
        ),
        (
            "ownership",
            [
                "python",
                "scripts/audit_diagnostic_ownership.py",
                "--all-realized",
                "--base",
                product_base_sha,
            ],
        ),
        (
            "generated-diff",
            [
                "git",
                "diff",
                "--exit-code",
                "--",
                "rust/crates/troupe-diagnostics-runtime/assets/generated",
                "frontend/diagnostics/package-lock.json",
            ],
        ),
    ]


def actual_command(
    command: Sequence[str],
    *,
    verify_dispatch: bool,
    index: int,
    name: str,
    timeout_seconds: int,
    evidence: Path,
) -> list[str]:
    if verify_dispatch:
        return [
            sys.executable,
            "-B",
            str(ROOT / "tests/unit/test_diagnostics_final_runner.py"),
            "--fake-child",
            str(index),
            name,
            str(timeout_seconds),
            str(evidence),
            "--",
            *command,
        ]
    result = list(command)
    if result[0].startswith("scripts/"):
        result[0] = str(ROOT / result[0])
    elif result[0] == "python":
        result[0] = sys.executable
    return result


def validate_source_reports(
    evidence: Path,
    integration_sha: str,
    cache_hashes: Mapping[str, str] | None,
) -> tuple[dict[str, Any], bytes, dict[str, Any], bytes]:
    performance_path = evidence / publisher.REPORT_NAMES["performance"]
    wheel_path = evidence / publisher.REPORT_NAMES["wheel"]
    performance, performance_payload = publisher._load_json(
        performance_path, "performance report"
    )
    wheel, wheel_payload = publisher._load_json(wheel_path, "wheel report")
    performance_schema, _ = publisher._load_json(
        publisher.PERFORMANCE_SCHEMA, "performance schema"
    )
    wheel_schema, _ = publisher._load_json(publisher.WHEEL_SCHEMA, "wheel schema")
    publisher.validate_schema(performance, performance_schema, "performance report")
    publisher.validate_schema(wheel, wheel_schema, "wheel report")
    identity = publisher._identity(integration_sha)
    if performance["identity"] != identity or wheel["identity"] != identity:
        fail("V05/V07 report identity differs from final integration")
    if (
        performance["kind"] != "gate"
        or performance["result"]["status"] != "passed"
        or performance["result"]["violations"] != []
        or performance["result"]["result_sha256"]
        != publisher._performance_result_sha(performance)
    ):
        fail("V05 performance report result is invalid")
    if (
        wheel["result"]["status"] != "passed"
        or wheel["result"]["result_sha256"] != publisher._result_sha(wheel)
    ):
        fail("V07 wheel report result is invalid")
    if cache_hashes is not None:
        observed = performance["environment"]["cache"]
        if observed["npm_cache_manifest_sha256"] != cache_hashes["npm"]:
            fail("V05 report npm cache identity differs from final cache")
        if observed["browser_cache_manifest_sha256"] != cache_hashes["playwright"]:
            fail("V05 report Playwright cache identity differs from final cache")
    return performance, performance_payload, wheel, wheel_payload


def final_report(
    namespace: argparse.Namespace,
    evidence: Path,
    child_results: list[dict[str, Any]],
    cache_hashes: Mapping[str, str] | None,
) -> dict[str, Any]:
    performance, performance_payload, wheel, wheel_payload = validate_source_reports(
        evidence, namespace.integration_sha, cache_hashes
    )
    performance_cache = sha256_bytes(
        publisher._canonical_json(performance["environment"]["cache"])
    )
    wheel_cache = sha256_bytes(publisher._canonical_json(wheel["cache"]))
    cache: dict[str, str] = {
        "npm_manifest_sha256": (
            cache_hashes["npm"]
            if cache_hashes is not None
            else performance["environment"]["cache"]["npm_cache_manifest_sha256"]
        ),
        "playwright_identity_sha256": (
            cache_hashes["playwright"]
            if cache_hashes is not None
            else performance["environment"]["cache"][
                "browser_cache_manifest_sha256"
            ]
        ),
        "perfetto_identity_sha256": (
            cache_hashes["perfetto"] if cache_hashes is not None else "8" * 64
        ),
        "performance_report_cache_sha256": performance_cache,
        "wheel_report_cache_sha256": wheel_cache,
    }
    cache["aggregate_sha256"] = sha256_bytes(publisher._canonical_json(cache))
    report: dict[str, Any] = {
        "schema": "troupe.diagnostics.final-evidence.v1",
        "attempt_id": namespace.attempt_id,
        "identity": publisher._identity(namespace.integration_sha),
        "cache": cache,
        "children": child_results,
        "reports": {
            "performance": {
                "path": publisher.REPORT_NAMES["performance"],
                "sha256": sha256_bytes(performance_payload),
                "result_sha256": performance["result"]["result_sha256"],
                "cache_sha256": performance_cache,
            },
            "wheel": {
                "path": publisher.REPORT_NAMES["wheel"],
                "sha256": sha256_bytes(wheel_payload),
                "result_sha256": wheel["result"]["result_sha256"],
                "cache_sha256": wheel_cache,
            },
        },
    }
    report["result"] = {
        "status": "passed",
        "result_sha256": publisher._result_sha(report),
    }
    schema, _ = publisher._load_json(publisher.FINAL_SCHEMA, "final evidence schema")
    publisher.validate_schema(report, schema, "final evidence")
    return report


def publish_final_report(path: Path, report: Mapping[str, Any]) -> None:
    if path.exists() or path.is_symlink():
        fail("V03 final evidence report must be create-new")
    try:
        publisher.atomic_publish(path, publisher._canonical_json(report) + b"\n")
    except publisher.AcceptanceError as error:
        fail(f"could not publish V03 final evidence: {error}")


def run() -> int:
    namespace = parse(ARGUMENTS)
    base, evidence, acceptance = prepare_paths(namespace)
    verify_dispatch = bool(namespace.verify_dispatch)
    cache_paths: dict[str, Path] = {}
    cache_hashes: dict[str, str] | None = None
    if not verify_dispatch:
        cache_paths["npm"], npm_sha = exact_cache(
            namespace.npm_cache, "npm cache", CACHE_IDENTITIES["npm"]
        )
        cache_paths["playwright"], playwright_sha = exact_cache(
            namespace.browser_cache,
            "Playwright cache",
            CACHE_IDENTITIES["playwright"],
        )
        cache_paths["perfetto"], perfetto_sha = exact_cache(
            namespace.perfetto_cache, "Perfetto cache", CACHE_IDENTITIES["perfetto"]
        )
        cache_hashes = {
            "npm": npm_sha,
            "playwright": playwright_sha,
            "perfetto": perfetto_sha,
        }

    environment = dict(os.environ)
    verify_zero_audit = not verify_dispatch or (
        environment.pop("TROUPE_FINAL_FAKE_ZERO_AUDIT", None) == "1"
    )
    if verify_dispatch:
        product_base_sha = "0" * 40
        plan_bundle_sha = "0" * 40
    else:
        product_base_sha = environment.get("PRODUCT_BASE_SHA", "0" * 40)
        plan_bundle_sha = environment.get("PLAN_BUNDLE_SHA", "0" * 40)
        if publisher.COMMIT_RE.fullmatch(product_base_sha) is None:
            fail("PRODUCT_BASE_SHA is required")
        if publisher.COMMIT_RE.fullmatch(plan_bundle_sha) is None:
            fail("PLAN_BUNDLE_SHA is required")
        head = subprocess.run(
            ["git", "rev-parse", "HEAD"],
            cwd=ROOT,
            check=True,
            capture_output=True,
            text=True,
        ).stdout.strip()
        branch = subprocess.run(
            ["git", "branch", "--show-current"],
            cwd=ROOT,
            check=True,
            capture_output=True,
            text=True,
        ).stdout.strip()
        if head != namespace.integration_sha or branch != "integration/production-diagnostics":
            fail("final runner requires the declared clean integration HEAD")

    initial_status = subprocess.run(
        ["git", "status", "--porcelain=v1", "--untracked-files=all", "-z"],
        cwd=ROOT,
        check=True,
        capture_output=True,
    ).stdout
    if not verify_dispatch and initial_status:
        fail("final runner requires a clean integration checkout")

    environment.update(
        {
            "INTEGRATION_SHA": namespace.integration_sha,
            "PLAN_BUNDLE_SHA": plan_bundle_sha,
            "PRODUCT_BASE_SHA": product_base_sha,
            "TROUPE_DIAGNOSTICS_EVIDENCE": str(base),
            "TROUPE_FINAL_ATTEMPT_ID": namespace.attempt_id,
        }
    )
    if cache_paths:
        environment.update(
            {
                "TROUPE_NPM_CACHE": str(cache_paths["npm"]),
                "TROUPE_PLAYWRIGHT_CACHE": str(cache_paths["playwright"]),
                "TROUPE_PERFETTO_CACHE": str(cache_paths["perfetto"]),
            }
        )

    temporary_base = Path(environment.get("TMPDIR", "/tmp")).resolve(strict=True)
    runner_tmp = Path(
        tempfile.mkdtemp(prefix="troupe-diagnostics-final.", dir=temporary_base)
    ).resolve(strict=True)
    marker = runner_tmp / ".troupe-final-owned"
    marker.write_text("owned\n", encoding="ascii")
    child_results: list[dict[str, Any]] = []
    first_failure = 0
    try:
        for index, ((name, command), timeout_seconds) in enumerate(
            zip(child_manifest(evidence, product_base_sha), TIMEOUTS, strict=True),
            start=1,
        ):
            stdout_path = runner_tmp / f"{index:02d}.stdout"
            stderr_path = runner_tmp / f"{index:02d}.stderr"
            actual = actual_command(
                command,
                verify_dispatch=verify_dispatch,
                index=index,
                name=name,
                timeout_seconds=timeout_seconds,
                evidence=evidence,
            )
            status = execute(
                actual,
                stdout_path=stdout_path,
                stderr_path=stderr_path,
                environment=environment,
                timeout_seconds=timeout_seconds,
            )
            if index == 11 and status == 0 and verify_zero_audit:
                try:
                    zero_audit(initial_status, runner_tmp)
                except (FinalRunnerError, OSError, subprocess.SubprocessError) as error:
                    append_error(stderr_path, str(error))
                    status = 1
            stream_file(stdout_path, sys.stdout)
            stream_file(stderr_path, sys.stderr)
            child = {
                "index": index,
                "name": name,
                "argv": command,
                "exit_code": status,
                "stdout_sha256": command_output_hash(stdout_path),
                "stderr_sha256": command_output_hash(stderr_path),
            }
            child["result_sha256"] = publisher._child_result_sha(child)
            child_results.append(child)
            if status != 0 and first_failure == 0:
                first_failure = status
        if first_failure != 0:
            return first_failure

        report_path = evidence / FINAL_REPORT_NAME
        report = final_report(
            namespace, evidence, child_results, cache_hashes
        )
        publish_final_report(report_path, report)

        publisher_command = [
            "python",
            "scripts/publish_diagnostics_acceptance.py",
            "--evidence-base",
            str(base),
            "--attempt-id",
            namespace.attempt_id,
            "--integration-sha",
            namespace.integration_sha,
            "--output",
            str(acceptance),
        ]
        if verify_dispatch:
            actual_publisher = [
                sys.executable,
                "-B",
                str(ROOT / "tests/unit/test_diagnostics_final_runner.py"),
                "--fake-publisher",
                "--",
                *publisher_command,
            ]
        else:
            actual_publisher = [sys.executable, str(PUBLISHER_PATH), *publisher_command[2:]]
        completed = subprocess.run(
            actual_publisher,
            cwd=ROOT,
            env=environment,
            check=False,
        )
        return completed.returncode
    finally:
        try:
            if (
                runner_tmp.parent == temporary_base
                and runner_tmp.name.startswith("troupe-diagnostics-final.")
                and marker.is_file()
            ):
                shutil.rmtree(runner_tmp)
            else:
                print(
                    f"final diagnostics runner: refusing to clean unowned temp {runner_tmp}",
                    file=sys.stderr,
                )
        except OSError as error:
            print(f"final diagnostics runner: cleanup failed: {error}", file=sys.stderr)


try:
    raise SystemExit(run())
except (FinalRunnerError, publisher.AcceptanceError) as error:
    print(f"final diagnostics runner: {error}", file=sys.stderr)
    raise SystemExit(1)
PY
