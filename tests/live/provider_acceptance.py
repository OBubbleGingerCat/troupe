from __future__ import annotations

import json
import os
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
CODEX_PRODUCTION = ROOT / "examples" / "live_agents" / "codex_actor"
PROFILE_ENV = "TROUPE_LIVE_CODEX_PROFILE"
CASE_HARD_STOP_SECONDS = 900.0
SHUTDOWN_HARD_STOP_SECONDS = 60.0


class AcceptanceFailure(RuntimeError):
    pass


def _load_profile(provider: str) -> tuple[Path, dict[str, object]]:
    if provider != "codex":
        raise AcceptanceFailure("unsupported live provider")
    raw = os.environ.get(PROFILE_ENV)
    if raw is None:
        raise AcceptanceFailure(f"{PROFILE_ENV} is required")
    try:
        value = json.loads(raw)
    except json.JSONDecodeError as error:
        raise AcceptanceFailure(f"{PROFILE_ENV} must contain valid JSON") from error
    if not isinstance(value, dict):
        raise AcceptanceFailure(f"{PROFILE_ENV} must contain a JSON object")
    workspace = value.get("workspace")
    model = value.get("model")
    if "effort" not in value:
        raise AcceptanceFailure(f"{PROFILE_ENV} must contain effort")
    effort = value["effort"]
    if not isinstance(workspace, str) or not workspace:
        raise AcceptanceFailure("live profile workspace must be a non-empty string")
    if not isinstance(model, str) or not model:
        raise AcceptanceFailure("live profile model must be a non-empty string")
    if effort is not None and (not isinstance(effort, str) or not effort):
        raise AcceptanceFailure("live profile effort must be a non-empty string or null")
    base = Path(workspace).expanduser().resolve(strict=True)
    if not base.is_dir():
        raise AcceptanceFailure("live profile workspace must be a directory")
    return base, {"workspace": str(base), "model": model, "effort": effort}


def _process_group_is_gone(process_group: int) -> bool:
    try:
        os.killpg(process_group, 0)
    except ProcessLookupError:
        return True
    return False


def _stop_process_group(process: subprocess.Popen[bytes]) -> None:
    if process.poll() is None:
        os.killpg(process.pid, signal.SIGINT)
        try:
            process.wait(timeout=SHUTDOWN_HARD_STOP_SECONDS)
        except subprocess.TimeoutExpired:
            os.killpg(process.pid, signal.SIGKILL)
            process.wait(timeout=SHUTDOWN_HARD_STOP_SECONDS)
    if not _process_group_is_gone(process.pid):
        os.killpg(process.pid, signal.SIGKILL)
        deadline = time.monotonic() + SHUTDOWN_HARD_STOP_SECONDS
        while not _process_group_is_gone(process.pid):
            if time.monotonic() >= deadline:
                raise AcceptanceFailure("live provider process group survived cleanup")
            time.sleep(0.05)


def _run_case(
    *,
    mode: str,
    workspace: Path,
    profile: dict[str, object],
    seed_token: str,
    unlogged: bool = False,
) -> dict[str, Any]:
    report = workspace / f"{mode}-report.json"
    stdout_log = workspace / f"{mode}-stdout.log"
    stderr_log = workspace / f"{mode}-stderr.log"
    environment = os.environ.copy()
    environment["PYTHONDONTWRITEBYTECODE"] = "1"
    environment["CODEX_PATH"] = str(workspace / "forbidden-codex-override")
    environment[PROFILE_ENV] = json.dumps(
        profile,
        sort_keys=True,
        separators=(",", ":"),
    )
    if unlogged:
        codex_home = workspace / "unlogged-codex-home"
        codex_home.mkdir()
        environment["CODEX_HOME"] = str(codex_home)
        environment["NO_BROWSER"] = "1"
        for name in ("CODEX_API_KEY", "OPENAI_API_KEY", "DEFAULT_AUTH_REQUEST"):
            environment.pop(name, None)

    console = Path(sys.executable).with_name("troupe")
    if not console.is_file():
        raise AcceptanceFailure("troupe console is unavailable")
    command = [
        str(console),
        "--production",
        str(CODEX_PRODUCTION),
        "--",
        mode,
        str(report),
        seed_token,
    ]
    with stdout_log.open("wb") as stdout, stderr_log.open("wb") as stderr:
        process = subprocess.Popen(
            command,
            cwd=ROOT,
            env=environment,
            stdin=subprocess.DEVNULL,
            stdout=stdout,
            stderr=stderr,
            start_new_session=True,
        )
        deadline = time.monotonic() + CASE_HARD_STOP_SECONDS
        try:
            while not report.is_file():
                return_code = process.poll()
                if return_code is not None:
                    raise AcceptanceFailure(
                        f"live {mode} case exited before publishing its report"
                    )
                if time.monotonic() >= deadline:
                    raise AcceptanceFailure(
                        f"live {mode} case exceeded its outer hard stop"
                    )
                time.sleep(0.05)
            try:
                payload = json.loads(report.read_text(encoding="utf-8"))
            except (OSError, json.JSONDecodeError) as error:
                raise AcceptanceFailure(f"live {mode} report is invalid") from error
        finally:
            _stop_process_group(process)
    if process.returncode != 0:
        raise AcceptanceFailure(f"live {mode} case did not shut down cleanly")
    if not isinstance(payload, dict):
        raise AcceptanceFailure(f"live {mode} report must be an object")
    return payload


def _require_error(
    payload: dict[str, Any],
    *,
    error_type: str,
    code: str,
    phase: str,
) -> None:
    expected = {
        "kind": "error",
        "type": error_type,
        "code": code,
        "phase": phase,
    }
    if payload != expected:
        raise AcceptanceFailure("live negative case returned the wrong typed error")


def _run_codex(base: Path, configured_profile: dict[str, object]) -> None:
    workspace = Path(tempfile.mkdtemp(prefix=".troupe-live-codex-", dir=base))
    ownership_marker = workspace / ".troupe-live-owned"
    ownership_marker.write_text("codex\n", encoding="ascii")
    seed_token = f"ctx-{secrets.token_hex(8)}"
    try:
        live_profile = {**configured_profile, "workspace": str(workspace)}
        (workspace / "seed.txt").write_text(seed_token + "\n", encoding="utf-8")
        acceptance = _run_case(
            mode="acceptance",
            workspace=workspace,
            profile=live_profile,
            seed_token=seed_token,
        )
        if acceptance.get("remember") != {"status": "stored", "token": seed_token}:
            raise AcceptanceFailure("Codex did not return the first contextual result")
        if acceptance.get("recall") != {"token": seed_token, "confidence": 8}:
            raise AcceptanceFailure("Codex did not retain context or repair custom schema")
        if acceptance.get("cancel") != {"cancelled": True}:
            raise AcceptanceFailure("Codex caller cancellation was not observed")
        recovery = acceptance.get("recover")
        reusable = recovery == {
            "kind": "result",
            "value": {"status": "recovered"},
        }
        typed_broken = (
            isinstance(recovery, dict)
            and recovery.get("kind") == "broken"
            and recovery.get("code")
            in {
                "authentication_lost",
                "process_exited",
                "protocol_violation",
                "transport_lost",
                "uncertain_settlement",
            }
        )
        if not reusable and not typed_broken:
            raise AcceptanceFailure("Codex cancellation settlement was not normalized")
        artifact = workspace / "artifact.txt"
        if artifact.read_text(encoding="utf-8").strip() != "codex-workspace-ok":
            raise AcceptanceFailure("Codex workspace side effect is missing or incorrect")
        if (workspace / "seed.txt").exists():
            raise AcceptanceFailure("context seed survived the first turn")

        invalid_model = {
            **live_profile,
            "model": f"troupe-invalid-model-{secrets.token_hex(8)}",
        }
        _require_error(
            _run_case(
                mode="invalid-model",
                workspace=workspace,
                profile=invalid_model,
                seed_token=seed_token,
            ),
            error_type="AgentSessionStartError",
            code="configuration_invalid",
            phase="configure",
        )

        invalid_effort = {
            **live_profile,
            "effort": f"troupe-invalid-effort-{secrets.token_hex(8)}",
        }
        _require_error(
            _run_case(
                mode="invalid-effort",
                workspace=workspace,
                profile=invalid_effort,
                seed_token=seed_token,
            ),
            error_type="AgentSessionStartError",
            code="configuration_invalid",
            phase="configure",
        )

        _require_error(
            _run_case(
                mode="auth-required",
                workspace=workspace,
                profile=live_profile,
                seed_token=seed_token,
                unlogged=True,
            ),
            error_type="AgentAuthenticationRequiredError",
            code="authentication_required",
            phase="session_new",
        )
    finally:
        if ownership_marker.read_text(encoding="ascii") != "codex\n":
            raise AcceptanceFailure("refusing to clean an unowned live workspace")
        shutil.rmtree(workspace)
    if workspace.exists():
        raise AcceptanceFailure("live Codex workspace survived cleanup")


def main(argv: list[str]) -> int:
    if argv != ["codex"]:
        raise AcceptanceFailure("usage: provider_acceptance.py codex")
    from troupe import _runtime

    if hasattr(_runtime, "_agent_test_reset_launch"):
        raise AcceptanceFailure("live acceptance requires a build without agent-test-support")
    base, profile = _load_profile("codex")
    _run_codex(base, profile)
    return 0


if __name__ == "__main__":
    try:
        raise SystemExit(main(sys.argv[1:]))
    except AcceptanceFailure as error:
        print(str(error), file=sys.stderr)
        raise SystemExit(1) from None
