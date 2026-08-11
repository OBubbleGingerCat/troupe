from __future__ import annotations

import importlib
import json
import os
import re
import secrets
import shutil
import signal
import subprocess
import sys
import tempfile
import time
from pathlib import Path
from typing import Any


ROOT = Path(__file__).resolve().parents[2]
PRODUCTION = ROOT / "examples" / "live_agents" / "mixed_repository_repair"
CASE_HARD_STOP_SECONDS = 900.0
PROCESS_CLEANUP_SECONDS = 60.0


support = importlib.import_module("provider_acceptance")
AcceptanceFailure = support.AcceptanceFailure


def _git(repository: Path, *args: str) -> str:
    result = subprocess.run(
        ["git", *args],
        cwd=repository,
        stdin=subprocess.DEVNULL,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        check=True,
        text=True,
    )
    return result.stdout.strip()


def _create_repository(repository: Path, investigation_id: str) -> tuple[str, bytes]:
    repository.mkdir()
    (repository / ".gitignore").write_text("__pycache__/\n", encoding="utf-8")
    (repository / "repair.py").write_text(
        "def normalize_title(value: str) -> str:\n"
        "    return value\n",
        encoding="utf-8",
    )
    test_source = (
        "import unittest\n\n"
        "from repair import normalize_title\n\n\n"
        "class RepairTests(unittest.TestCase):\n"
        "    def test_normalizes_title(self) -> None:\n"
        "        self.assertEqual(\n"
        "            normalize_title(\"  a tale OF TWO cities  \"),\n"
        "            \"A Tale Of Two Cities\",\n"
        "        )\n\n\n"
        "if __name__ == \"__main__\":\n"
        "    unittest.main()\n"
    ).encode("utf-8")
    (repository / "test_repair.py").write_bytes(test_source)
    _git(repository, "init", "-q")
    _git(repository, "config", "user.name", "Troupe Live Acceptance")
    _git(repository, "config", "user.email", "troupe@example.invalid")
    _git(repository, "add", ".gitignore", "repair.py", "test_repair.py")
    _git(repository, "commit", "-q", "-m", "baseline")
    baseline = _git(repository, "rev-parse", "HEAD")
    (repository / "ISSUE.md").write_text(
        f"investigation-id: {investigation_id}\n"
        "normalize_title must strip surrounding whitespace and title-case each word.\n",
        encoding="utf-8",
    )
    return baseline, test_source


def _process_identity(pid: int) -> str | None:
    try:
        fields = Path(f"/proc/{pid}/stat").read_text(encoding="ascii")
    except (FileNotFoundError, PermissionError, ProcessLookupError):
        return None
    suffix = fields.rsplit(")", 1)
    if len(suffix) != 2:
        return None
    values = suffix[1].split()
    return values[19] if len(values) > 19 else None


def _descendant_identities(root_pid: int) -> dict[int, str]:
    identities: dict[int, str] = {}
    pending = [root_pid]
    while pending:
        pid = pending.pop(0)
        if pid in identities:
            continue
        identity = _process_identity(pid)
        if identity is None:
            continue
        identities[pid] = identity
        pending.extend(support._process_children(pid))
    return identities


def _workspace_processes(repository: Path) -> list[int]:
    identity = repository.stat()
    matches = []
    for entry in Path("/proc").iterdir():
        if not entry.name.isdigit() or int(entry.name) == os.getpid():
            continue
        try:
            candidate = entry.joinpath("cwd").stat()
        except (FileNotFoundError, PermissionError, ProcessLookupError):
            continue
        if (candidate.st_dev, candidate.st_ino) == (identity.st_dev, identity.st_ino):
            matches.append(int(entry.name))
    return matches


def _require_process_cleanup(
    identities: dict[int, str],
    repository: Path,
) -> None:
    def leaked_processes() -> list[int]:
        retained = [
            pid
            for pid, identity in identities.items()
            if _process_identity(pid) == identity
        ]
        return sorted(set(retained + _workspace_processes(repository)))

    deadline = time.monotonic() + PROCESS_CLEANUP_SECONDS
    while True:
        leaked = leaked_processes()
        if not leaked:
            return
        if time.monotonic() >= deadline:
            for pid in leaked:
                try:
                    os.kill(pid, signal.SIGKILL)
                except ProcessLookupError:
                    pass
            forced_deadline = time.monotonic() + PROCESS_CLEANUP_SECONDS
            while leaked_processes():
                if time.monotonic() >= forced_deadline:
                    raise AcceptanceFailure(
                        "mixed live agent process survived forced cleanup"
                    )
                time.sleep(0.05)
            raise AcceptanceFailure("mixed live agent required forced cleanup")
        time.sleep(0.05)


def _prepare_claude_settings(workspace: Path) -> tuple[Path, Path, bytes]:
    config_dir = Path(
        os.environ.get("CLAUDE_CONFIG_DIR", str(Path.home() / ".claude"))
    ).expanduser()
    if not config_dir.is_absolute() or not config_dir.is_dir():
        raise AcceptanceFailure("Claude config directory is unavailable")
    target = config_dir / "settings.json"
    if not target.is_file():
        raise AcceptanceFailure("Claude user settings file is unavailable")
    original = target.read_bytes()
    source = workspace / "isolated-claude-settings.json"
    support._write_json(
        source,
        support._isolated_claude_user_settings(original, {}),
    )
    return source, target, original


def _production_command(
    *,
    workspace: Path,
    repository: Path,
    report: Path,
    profiles: dict[str, dict[str, object]],
) -> tuple[list[str], dict[str, str], tuple[Path, bytes]]:
    console = Path(sys.executable).with_name("troupe")
    if not console.is_file():
        raise AcceptanceFailure("troupe console is unavailable")
    bubblewrap = shutil.which("bwrap")
    if bubblewrap is None:
        raise AcceptanceFailure("mixed live acceptance requires bubblewrap")

    source_home = Path(
        os.environ.get("KIMI_CODE_HOME", str(Path.home() / ".kimi-code"))
    ).expanduser()
    if not source_home.is_absolute():
        raise AcceptanceFailure("KIMI_CODE_HOME must be absolute for live isolation")
    kimi_binary = support._resolve_kimi_binary(source_home)
    kimi_home = workspace / "isolated-kimi-home"
    support._copy_kimi_login(source_home, kimi_home)
    kimi_command_dir = support._prepare_kimi_command(workspace, kimi_binary)

    environment = os.environ.copy()
    environment["PYTHONDONTWRITEBYTECODE"] = "1"
    environment["CODEX_PATH"] = str(workspace / "forbidden-codex-override")
    environment["KIMI_CODE_HOME"] = str(kimi_home)
    environment["KIMI_CODE_NO_AUTO_UPDATE"] = "1"
    environment["KIMI_DISABLE_TELEMETRY"] = "1"
    environment["KIMI_CODE_BACKGROUND_KEEP_ALIVE_ON_EXIT"] = "0"
    environment["PATH"] = os.pathsep.join(
        (str(kimi_command_dir), environment.get("PATH", ""))
    )
    for provider, profile in profiles.items():
        environment[support.PROFILE_ENVS[provider]] = json.dumps(
            {**profile, "workspace": str(repository)},
            sort_keys=True,
            separators=(",", ":"),
        )

    production_command = [
        str(console),
        "--production",
        str(PRODUCTION),
        "--",
        str(repository),
        str(report),
    ]
    command = [
        bubblewrap,
        "--die-with-parent",
        "--bind",
        "/",
        "/",
        "--dev-bind",
        "/dev",
        "/dev",
        "--proc",
        "/proc",
    ]
    user_settings, user_settings_target, original_user_settings = (
        _prepare_claude_settings(workspace)
    )
    command.extend(["--ro-bind", str(user_settings), str(user_settings_target)])
    command.extend(["--", *production_command])
    return command, environment, (user_settings_target, original_user_settings)


def _run_production(
    *,
    workspace: Path,
    repository: Path,
    report: Path,
    profiles: dict[str, dict[str, object]],
) -> dict[str, Any]:
    command, environment, user_settings_audit = _production_command(
        workspace=workspace,
        repository=repository,
        report=report,
        profiles=profiles,
    )
    stdout_log = workspace / "stdout.log"
    stderr_log = workspace / "stderr.log"
    identities: dict[int, str] = {}
    payload: object = None
    process: subprocess.Popen[bytes] | None = None
    run_error: BaseException | None = None
    try:
        with stdout_log.open("wb") as stdout, stderr_log.open("wb") as stderr:
            try:
                process = subprocess.Popen(
                    command,
                    cwd=ROOT,
                    env=environment,
                    stdin=subprocess.DEVNULL,
                    stdout=stdout,
                    stderr=stderr,
                    start_new_session=True,
                )
            except OSError as error:
                raise AcceptanceFailure(
                    "mixed live Production could not start"
                ) from error
            deadline = time.monotonic() + CASE_HARD_STOP_SECONDS
            try:
                while not report.is_file():
                    identities.update(_descendant_identities(process.pid))
                    return_code = process.poll()
                    if return_code is not None:
                        detail = support._stderr_tail(stderr_log)
                        suffix = f": {detail}" if detail else ""
                        raise AcceptanceFailure(
                            "mixed live Production exited before publishing its report"
                            + suffix
                        )
                    if time.monotonic() >= deadline:
                        raise AcceptanceFailure(
                            "mixed live Production exceeded its outer hard stop"
                        )
                    time.sleep(0.05)
                try:
                    payload = json.loads(report.read_text(encoding="utf-8"))
                except (OSError, json.JSONDecodeError) as error:
                    raise AcceptanceFailure("mixed live report is invalid") from error
                identities.update(_descendant_identities(process.pid))
            finally:
                graceful_pid = None
                if process.poll() is None:
                    try:
                        graceful_pid = support._find_troupe_descendant(process.pid)
                    except AcceptanceFailure:
                        pass
                support._stop_process_group(process, graceful_pid=graceful_pid)
    except BaseException as error:
        run_error = error
    user_settings_target, original_user_settings = user_settings_audit
    settings_error: BaseException | None = None
    try:
        current_user_settings = (
            user_settings_target.read_bytes() if user_settings_target.is_file() else None
        )
    except BaseException as error:
        settings_error = error
    else:
        if current_user_settings != original_user_settings:
            settings_error = AcceptanceFailure(
                "mixed live isolation modified Claude user settings"
            )
    cleanup_error: BaseException | None = None
    if process is not None:
        try:
            _require_process_cleanup(identities, repository)
        except BaseException as error:
            cleanup_error = error
    if cleanup_error is not None:
        raise cleanup_error
    if settings_error is not None:
        raise settings_error
    if run_error is not None:
        raise run_error
    if process is None:
        raise AssertionError("mixed live process was not created")
    if process.returncode != 0:
        detail = support._stderr_tail(stderr_log)
        suffix = f": {detail}" if detail else ""
        raise AcceptanceFailure(
            f"mixed live Production did not shut down cleanly{suffix}"
        )
    if not isinstance(payload, dict):
        raise AcceptanceFailure("mixed live report must be an object")
    return payload


def _require_exact(value: object, expected: object, label: str) -> None:
    if value != expected:
        raise AcceptanceFailure(f"mixed live {label} is incorrect")


def _verify_result(
    *,
    payload: dict[str, Any],
    repository: Path,
    baseline: str,
    investigation_id: str,
    test_source: bytes,
) -> None:
    _require_exact(
        payload.get("investigation"),
        {
            "expected_behavior": "strip surrounding whitespace and title-case each word",
            "investigation_id": investigation_id,
            "role": "investigator",
            "root_cause": "normalize_title returns its input unchanged",
            "target_file": "repair.py",
        },
        "investigation result",
    )
    _require_exact(
        payload.get("review"),
        {
            "approved": True,
            "contract": {
                "input": "arbitrary title text",
                "output": "trimmed title-cased text",
            },
            "role": "reviewer",
        },
        "review result",
    )
    _require_exact(
        payload.get("recall"),
        {
            "investigation_id": investigation_id,
            "remembered_root_cause": "normalize_title returns its input unchanged",
            "role": "investigator",
        },
        "context recall",
    )
    repair = payload.get("repair")
    if not isinstance(repair, dict):
        raise AcceptanceFailure("mixed live repair result is not an object")
    _require_exact(repair.get("role"), "implementer", "repair role")
    _require_exact(repair.get("changed_files"), ["repair.py"], "changed files")
    _require_exact(repair.get("tests_passed"), True, "test status")
    _require_exact(
        payload.get("flow"),
        [
            {"effect": "Investigation", "owner": "codex-investigator"},
            {"effect": "ContractReview", "owner": "claude-reviewer"},
            {"effect": "RepositoryRepair", "owner": "kimi-repairer"},
            {"effect": "ContextRecall", "owner": "codex-investigator"},
        ],
        "Effect flow",
    )

    head = _git(repository, "rev-parse", "HEAD")
    commit = repair.get("commit")
    if not isinstance(commit, str) or re.fullmatch(r"[0-9a-f]{40}", commit) is None:
        raise AcceptanceFailure("mixed live repair commit is not a full Git SHA")
    _require_exact(commit, head, "repair commit")
    _require_exact(
        _git(repository, "rev-list", "--count", f"{baseline}..HEAD"),
        "1",
        "commit count",
    )
    _require_exact(
        _git(repository, "diff-tree", "--no-commit-id", "--name-only", "-r", "HEAD"),
        "repair.py",
        "commit file set",
    )
    _require_exact(
        _git(repository, "log", "-1", "--pretty=%s"),
        "fix: normalize titles",
        "commit message",
    )
    _require_exact(
        _git(repository, "status", "--porcelain"),
        "",
        "repository status",
    )
    if (repository / "ISSUE.md").exists():
        raise AcceptanceFailure("mixed live investigation source survived context handoff")
    if (repository / "test_repair.py").read_bytes() != test_source:
        raise AcceptanceFailure("mixed live repair modified the behavior test")

    try:
        subprocess.run(
            [sys.executable, "-m", "unittest", "-q"],
            cwd=repository,
            env={**os.environ, "PYTHONDONTWRITEBYTECODE": "1"},
            stdin=subprocess.DEVNULL,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            check=True,
        )
    except subprocess.CalledProcessError as error:
        raise AcceptanceFailure("mixed live repository tests failed") from error
    _require_exact(_git(repository, "status", "--porcelain"), "", "post-test status")


def main(argv: list[str]) -> int:
    if argv:
        raise AcceptanceFailure("usage: mixed_agent_oracle.py")
    _runtime = importlib.import_module("troupe._runtime")
    if hasattr(_runtime, "_agent_test_reset_launch"):
        raise AcceptanceFailure("mixed live acceptance requires a release-feature build")
    loaded = {provider: support._load_profile(provider) for provider in support.PROFILE_ENVS}
    base = loaded["codex"][0]
    profiles = {provider: profile for provider, (_, profile) in loaded.items()}
    workspace = Path(tempfile.mkdtemp(prefix=".troupe-live-mixed-", dir=base))
    ownership_marker = workspace / ".troupe-live-owned"
    ownership_marker.write_text("mixed\n", encoding="ascii")
    repository = workspace / "repository"
    try:
        investigation_id = f"investigation-{secrets.token_hex(8)}"
        baseline, test_source = _create_repository(repository, investigation_id)
        payload = _run_production(
            workspace=workspace,
            repository=repository,
            report=workspace / "report.json",
            profiles=profiles,
        )
        _verify_result(
            payload=payload,
            repository=repository,
            baseline=baseline,
            investigation_id=investigation_id,
            test_source=test_source,
        )
    finally:
        if ownership_marker.read_text(encoding="ascii") != "mixed\n":
            raise AcceptanceFailure("refusing to clean an unowned mixed workspace")
        shutil.rmtree(workspace)
    if workspace.exists():
        raise AcceptanceFailure("mixed live workspace survived cleanup")
    print("mixed live acceptance passed")
    return 0


if __name__ == "__main__":
    try:
        raise SystemExit(main(sys.argv[1:]))
    except AcceptanceFailure as error:
        print(str(error), file=sys.stderr)
        raise SystemExit(1) from None
