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
PRODUCTIONS = {
    "codex": ROOT / "examples" / "live_agents" / "codex_actor",
    "claude": ROOT / "examples" / "live_agents" / "claude_actor",
}
PROFILE_ENVS = {
    "codex": "TROUPE_LIVE_CODEX_PROFILE",
    "claude": "TROUPE_LIVE_CLAUDE_PROFILE",
}
CLAUDE_USER_ENV_ALLOWLIST = frozenset(
    {
        "ANTHROPIC_API_KEY",
        "ANTHROPIC_AUTH_TOKEN",
        "ANTHROPIC_BASE_URL",
        "CLAUDE_API_KEY",
        "CLAUDE_CODE_OAUTH_TOKEN",
    }
)
CASE_HARD_STOP_SECONDS = 900.0
SHUTDOWN_HARD_STOP_SECONDS = 60.0


class AcceptanceFailure(RuntimeError):
    pass


def _stderr_tail(path: Path) -> str:
    if not path.is_file():
        return ""
    value = path.read_text(encoding="utf-8", errors="replace")[-2_000:].strip()
    return " ".join(value.splitlines())


def _load_profile(provider: str) -> tuple[Path, dict[str, object]]:
    if provider not in PROFILE_ENVS:
        raise AcceptanceFailure("unsupported live provider")
    profile_env = PROFILE_ENVS[provider]
    raw = os.environ.get(profile_env)
    if raw is None:
        raise AcceptanceFailure(f"{profile_env} is required")
    try:
        value = json.loads(raw)
    except json.JSONDecodeError as error:
        raise AcceptanceFailure(f"{profile_env} must contain valid JSON") from error
    if not isinstance(value, dict):
        raise AcceptanceFailure(f"{profile_env} must contain a JSON object")
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


def _process_children(pid: int) -> list[int]:
    path = Path(f"/proc/{pid}/task/{pid}/children")
    try:
        value = path.read_text(encoding="ascii").strip()
    except (FileNotFoundError, ProcessLookupError):
        return []
    return [int(child) for child in value.split()]


def _processes_with_argument(argument: str) -> list[int]:
    encoded = argument.encode("utf-8")
    matches = []
    for entry in Path("/proc").iterdir():
        if not entry.name.isdigit():
            continue
        try:
            arguments = entry.joinpath("cmdline").read_bytes().split(b"\0")
        except (FileNotFoundError, PermissionError, ProcessLookupError):
            continue
        if encoded in arguments:
            matches.append(int(entry.name))
    return matches


def _find_troupe_descendant(root_pid: int) -> int:
    pending = _process_children(root_pid)
    while pending:
        pid = pending.pop(0)
        try:
            fields = Path(f"/proc/{pid}/cmdline").read_bytes().split(b"\0")
        except (FileNotFoundError, ProcessLookupError):
            continue
        if b"--production" in fields and any(
            Path(os.fsdecode(field)).name == "troupe" for field in fields if field
        ):
            return pid
        pending.extend(_process_children(pid))
    raise AcceptanceFailure("could not identify the namespaced Troupe process")


def _stop_process_group(
    process: subprocess.Popen[bytes],
    *,
    graceful_pid: int | None = None,
) -> None:
    if process.poll() is None:
        if graceful_pid is None:
            os.killpg(process.pid, signal.SIGINT)
        else:
            try:
                os.kill(graceful_pid, signal.SIGINT)
            except ProcessLookupError:
                pass
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
    provider: str,
    mode: str,
    workspace: Path,
    profile: dict[str, object],
    seed_token: str,
    unlogged: bool = False,
    user_settings: Path | None = None,
    user_settings_target: Path | None = None,
) -> dict[str, Any]:
    report = workspace / f"{mode}-report.json"
    stdout_log = workspace / f"{mode}-stdout.log"
    stderr_log = workspace / f"{mode}-stderr.log"
    environment = os.environ.copy()
    environment["PYTHONDONTWRITEBYTECODE"] = "1"
    if provider == "codex":
        environment["CODEX_PATH"] = str(workspace / "forbidden-codex-override")
    elif provider == "claude":
        environment["TROUPE_CLAUDE_SETTING_PRECEDENCE"] = "ambient"
    else:
        raise AcceptanceFailure("unsupported live provider")
    environment[PROFILE_ENVS[provider]] = json.dumps(
        profile,
        sort_keys=True,
        separators=(",", ":"),
    )
    if unlogged:
        provider_home = workspace / f"unlogged-{provider}-home"
        provider_home.mkdir()
        if provider == "codex":
            environment["CODEX_HOME"] = str(provider_home)
        else:
            environment["CLAUDE_CONFIG_DIR"] = str(provider_home)
        environment["NO_BROWSER"] = "1"
        environment["BROWSER"] = "/bin/false"
        for name in (
            "ANTHROPIC_API_KEY",
            "ANTHROPIC_AUTH_TOKEN",
            "CLAUDE_API_KEY",
            "CLAUDE_CODE_OAUTH_TOKEN",
            "CODEX_API_KEY",
            "DEFAULT_AUTH_REQUEST",
            "OPENAI_API_KEY",
        ):
            environment.pop(name, None)

    console = Path(sys.executable).with_name("troupe")
    if not console.is_file():
        raise AcceptanceFailure("troupe console is unavailable")
    production_command = [
        str(console),
        "--production",
        str(PRODUCTIONS[provider]),
        "--",
        mode,
        str(report),
        seed_token,
    ]
    command = production_command
    if provider == "claude":
        if user_settings is None or user_settings_target is None:
            raise AcceptanceFailure("Claude live case requires isolated user settings")
        bubblewrap = shutil.which("bwrap")
        if bubblewrap is None:
            raise AcceptanceFailure("Claude live acceptance requires bubblewrap")
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
            "--ro-bind",
            str(user_settings),
            str(user_settings_target),
            "--",
            *production_command,
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
                    detail = _stderr_tail(stderr_log)
                    suffix = f": {detail}" if detail else ""
                    raise AcceptanceFailure(
                        f"live {mode} case exited before publishing its report{suffix}"
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
            graceful_pid = (
                _find_troupe_descendant(process.pid)
                if provider == "claude" and process.poll() is None
                else None
            )
            _stop_process_group(process, graceful_pid=graceful_pid)
    if process.returncode != 0:
        detail = _stderr_tail(stderr_log)
        suffix = f": {detail}" if detail else ""
        raise AcceptanceFailure(
            f"live {mode} case did not shut down cleanly "
            f"(returncode={process.returncode}){suffix}"
        )
    if not isinstance(payload, dict):
        raise AcceptanceFailure(f"live {mode} report must be an object")
    if unlogged:
        logs = (
            stdout_log.read_text(encoding="utf-8", errors="replace")
            + stderr_log.read_text(encoding="utf-8", errors="replace")
        ).lower()
        forbidden_interaction = (
            "claude.ai/login",
            "device code",
            "open your browser",
            "press enter",
            "setup-token",
        )
        if any(marker in logs for marker in forbidden_interaction):
            raise AcceptanceFailure("live auth failure attempted interactive login")
    return payload


def _require_error(
    payload: dict[str, Any],
    *,
    error_type: str,
    code: str,
    phase: str | None,
) -> None:
    expected = {
        "kind": "error",
        "type": error_type,
        "code": code,
    }
    if phase is not None:
        expected["phase"] = phase
    if payload != expected:
        raise AcceptanceFailure(
            "live negative case returned the wrong typed error: "
            f"expected={expected!r}, observed={payload!r}"
        )


def _run_codex(base: Path, configured_profile: dict[str, object]) -> None:
    workspace = Path(tempfile.mkdtemp(prefix=".troupe-live-codex-", dir=base))
    ownership_marker = workspace / ".troupe-live-owned"
    ownership_marker.write_text("codex\n", encoding="ascii")
    seed_token = f"ctx-{secrets.token_hex(8)}"
    try:
        live_profile = {**configured_profile, "workspace": str(workspace)}
        (workspace / "seed.txt").write_text(seed_token + "\n", encoding="utf-8")
        acceptance = _run_case(
            provider="codex",
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
                provider="codex",
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
                provider="codex",
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
                provider="codex",
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


def _command_hook(
    script: str,
    *args: str,
    once: bool = True,
) -> dict[str, object]:
    hook: dict[str, object] = {
        "type": "command",
        "command": "/bin/sh",
        "args": ["-c", script, "troupe-hook", *args],
    }
    if once:
        hook["once"] = True
    return hook


def _correction_audit_hook(path: Path) -> dict[str, object]:
    return _command_hook(
        'umask 077; /bin/cat >> "$1"; printf "\\n" >> "$1"',
        str(path),
        once=False,
    )


def _claude_settings(
    *,
    source: str,
    marker: Path,
    precedence_marker: Path | None = None,
    correction_audit: Path | None = None,
) -> dict[str, object]:
    hooks = [
        _command_hook(
            'printf "%s\\n" "$1" > "$2"',
            source,
            str(marker),
        )
    ]
    if precedence_marker is not None:
        hooks.append(
            _command_hook(
                'printf "%s\\n" "$TROUPE_CLAUDE_SETTING_PRECEDENCE" > "$1"',
                str(precedence_marker),
            )
        )
    post_tool_use: list[dict[str, object]] = [
        {
            "matcher": "Write",
            "hooks": hooks,
        }
    ]
    value: dict[str, object] = {
        "env": {"TROUPE_CLAUDE_SETTING_PRECEDENCE": source},
        "hooks": {"PostToolUse": post_tool_use},
    }
    if correction_audit is not None:
        result_matcher = "mcp__.*__troupe_submit_result"
        post_tool_use.append(
            {
                "matcher": result_matcher,
                "hooks": [_correction_audit_hook(correction_audit)],
            }
        )
        cast_hooks = value["hooks"]
        assert isinstance(cast_hooks, dict)
        cast_hooks["PostToolUseFailure"] = [
            {
                "matcher": result_matcher,
                "hooks": [_correction_audit_hook(correction_audit)],
            }
        ]
        cast_hooks["PreToolUse"] = [
            {"hooks": [_correction_audit_hook(correction_audit)]}
        ]
    if source == "local":
        value["permissions"] = {"ask": ["Write"]}
    return value


def _load_claude_correction_audit(path: Path) -> list[dict[str, object]]:
    if not path.is_file():
        raise AcceptanceFailure("Claude schema correction audit is missing")
    events = []
    for line_number, line in enumerate(
        path.read_text(encoding="utf-8").splitlines(),
        start=1,
    ):
        if not line.strip():
            continue
        try:
            event = json.loads(line)
        except json.JSONDecodeError as error:
            raise AcceptanceFailure(
                f"Claude schema correction audit line {line_number} is invalid"
            ) from error
        if not isinstance(event, dict):
            raise AcceptanceFailure(
                f"Claude schema correction audit line {line_number} is not an object"
            )
        events.append(event)
    return events


def _nested_text(value: object) -> str:
    if isinstance(value, str):
        return value
    if isinstance(value, dict):
        return "\n".join(_nested_text(item) for item in value.values())
    if isinstance(value, list):
        return "\n".join(_nested_text(item) for item in value)
    return ""


def _claude_result_event_kind(event: dict[str, object]) -> str | None:
    name = event.get("tool_name")
    if not _is_claude_result_tool(name):
        return None
    hook_event = event.get("hook_event_name")
    if hook_event == "PostToolUseFailure":
        return (
            "invalid"
            if "result validation failed" in _nested_text(event.get("error"))
            else None
        )
    if hook_event != "PostToolUse":
        return None
    response = event.get("tool_response")
    if not isinstance(response, (dict, list)):
        return None
    response_text = _nested_text(response)
    is_error = (
        response.get("is_error", response.get("isError"))
        if isinstance(response, dict)
        else False
    )
    if is_error is True and "result validation failed" in response_text:
        return "invalid"
    if is_error is not True and "result accepted" in response_text:
        return "accepted"
    return None


def _is_claude_result_tool(name: object) -> bool:
    return (
        isinstance(name, str)
        and name.startswith("mcp__")
        and name.endswith("__troupe_submit_result")
    )


def _claude_result_value(event: dict[str, object]) -> dict[str, object] | None:
    tool_input = event.get("tool_input")
    if not isinstance(tool_input, dict):
        return None
    value = tool_input.get("value")
    return value if isinstance(value, dict) else None


def _require_claude_schema_corrections(
    events: list[dict[str, object]],
    *,
    seed_token: str,
) -> None:
    observed: list[tuple[str, dict[str, object]]] = []
    for event in events:
        kind = _claude_result_event_kind(event)
        value = _claude_result_value(event)
        if kind is not None and value is not None:
            observed.append((kind, value))

    expected = [
        ("invalid", {"status": "needs-human", "token": seed_token}),
        ("accepted", {"status": "stored", "token": seed_token}),
        ("invalid", {"token": seed_token, "confidence": 6}),
        ("accepted", {"token": seed_token, "confidence": 8}),
    ]
    position = 0
    for evidence in observed:
        if evidence == expected[position]:
            position += 1
            if position == len(expected):
                return
    raise AcceptanceFailure(
        "Claude schema correction audit did not prove both ordered repairs: "
        f"observed={observed!r}"
    )


def _require_claude_context_recall(
    events: list[dict[str, object]],
    *,
    seed_token: str,
) -> None:
    remember_value = {"status": "stored", "token": seed_token}
    recall_value = {"token": seed_token, "confidence": 8}
    remember_index: int | None = None
    recall_index: int | None = None
    for index, event in enumerate(events):
        if _claude_result_event_kind(event) != "accepted":
            continue
        value = _claude_result_value(event)
        if remember_index is None and value == remember_value:
            remember_index = index
        elif remember_index is not None and value == recall_value:
            recall_index = index
            break
    if remember_index is None or recall_index is None:
        raise AcceptanceFailure("Claude context recall evidence is incomplete")

    first_turn_tools = {
        event.get("tool_name")
        for event in events[:remember_index]
        if event.get("hook_event_name") == "PreToolUse"
    }
    if not {"Read", "Write"}.issubset(first_turn_tools):
        raise AcceptanceFailure("Claude context recall tool audit is incomplete")

    recall_events = events[remember_index + 1 : recall_index]
    if any(
        event.get("hook_event_name") == "PreToolUse"
        and not _is_claude_result_tool(event.get("tool_name"))
        for event in recall_events
    ):
        raise AcceptanceFailure("Claude context recall used an external tool")

    recall_submissions = {
        value.get("confidence")
        for event in recall_events
        if event.get("hook_event_name") == "PreToolUse"
        and _is_claude_result_tool(event.get("tool_name"))
        and (value := _claude_result_value(event)) is not None
        and value.get("token") == seed_token
    }
    if not {6, 8}.issubset(recall_submissions):
        raise AcceptanceFailure("Claude context recall result audit is incomplete")


def _write_json(path: Path, value: object) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    payload = (
        json.dumps(value, sort_keys=True, separators=(",", ":")) + "\n"
    ).encode("utf-8")
    descriptor = os.open(path, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
    try:
        os.write(descriptor, payload)
    finally:
        os.close(descriptor)


def _isolated_claude_user_settings(
    original: bytes | None,
    fixture: dict[str, object],
) -> dict[str, object]:
    value = dict(fixture)
    fixture_env = fixture.get("env", {})
    if not isinstance(fixture_env, dict):
        raise AcceptanceFailure("Claude user settings fixture env must be an object")
    preserved_env: dict[str, object] = {}
    if original is not None:
        try:
            parsed = json.loads(original)
        except (UnicodeDecodeError, json.JSONDecodeError) as error:
            raise AcceptanceFailure("real Claude user settings are invalid") from error
        if not isinstance(parsed, dict):
            raise AcceptanceFailure("real Claude user settings must be an object")
        original_env = parsed.get("env", {})
        if not isinstance(original_env, dict):
            raise AcceptanceFailure("real Claude user settings env must be an object")
        preserved_env = {
            key: item
            for key, item in original_env.items()
            if key in CLAUDE_USER_ENV_ALLOWLIST
        }
    value["env"] = {**preserved_env, **fixture_env}
    return value


def _run_claude(base: Path, configured_profile: dict[str, object]) -> None:
    workspace = Path(tempfile.mkdtemp(prefix=".troupe-live-claude-", dir=base))
    ownership_marker = workspace / ".troupe-live-owned"
    ownership_marker.write_text("claude\n", encoding="ascii")
    seed_token = f"ctx-{secrets.token_hex(8)}"
    config_dir = Path(
        os.environ.get("CLAUDE_CONFIG_DIR", str(Path.home() / ".claude"))
    ).expanduser()
    if not config_dir.is_absolute():
        raise AcceptanceFailure("CLAUDE_CONFIG_DIR must be absolute for live isolation")
    user_settings_target = config_dir / "settings.json"
    if not config_dir.is_dir():
        raise AcceptanceFailure("Claude config directory is unavailable")
    original_user_settings = (
        user_settings_target.read_bytes() if user_settings_target.is_file() else None
    )
    user_settings = workspace / "isolated-user-settings.json"
    markers = {
        source: workspace / f"{source}-hook.txt"
        for source in ("user", "project", "local")
    }
    precedence_marker = workspace / "settings-precedence.txt"
    correction_audit = workspace / "schema-correction-audit.jsonl"
    try:
        live_profile = {**configured_profile, "workspace": str(workspace)}
        (workspace / "seed.txt").write_text(seed_token + "\n", encoding="utf-8")
        _write_json(
            user_settings,
            _isolated_claude_user_settings(
                original_user_settings,
                _claude_settings(source="user", marker=markers["user"]),
            ),
        )
        _write_json(
            workspace / ".claude" / "settings.json",
            _claude_settings(source="project", marker=markers["project"]),
        )
        _write_json(
            workspace / ".claude" / "settings.local.json",
            _claude_settings(
                source="local",
                marker=markers["local"],
                precedence_marker=precedence_marker,
                correction_audit=correction_audit,
            ),
        )

        acceptance = _run_case(
            provider="claude",
            mode="acceptance",
            workspace=workspace,
            profile=live_profile,
            seed_token=seed_token,
            user_settings=user_settings,
            user_settings_target=user_settings_target,
        )
        if acceptance.get("remember") != {"status": "stored", "token": seed_token}:
            raise AcceptanceFailure("Claude did not return the first contextual result")
        if acceptance.get("recall") != {"token": seed_token, "confidence": 8}:
            raise AcceptanceFailure("Claude did not retain context or repair custom schema")
        _require_claude_schema_corrections(
            _load_claude_correction_audit(correction_audit),
            seed_token=seed_token,
        )
        _require_claude_context_recall(
            _load_claude_correction_audit(correction_audit),
            seed_token=seed_token,
        )
        if acceptance.get("cancel") != {"cancelled": True}:
            raise AcceptanceFailure("Claude caller cancellation was not observed")
        if acceptance.get("recover") != {
            "kind": "broken",
            "code": "uncertain_settlement",
        }:
            raise AcceptanceFailure("Claude synthetic cancellation was treated as reusable")
        cancel_marker = workspace / "cancel-started.txt"
        try:
            cancel_pid = int(cancel_marker.read_text(encoding="ascii").strip())
        except (OSError, ValueError) as error:
            raise AcceptanceFailure("Claude cancellation marker is invalid") from error
        if cancel_pid <= 0:
            raise AcceptanceFailure("Claude cancellation marker has an invalid PID")
        cancel_process_name = f"troupe-claude-cancel-{seed_token}"
        leaked_processes = _processes_with_argument(cancel_process_name)
        if leaked_processes:
            for pid in leaked_processes:
                try:
                    os.kill(pid, signal.SIGKILL)
                except ProcessLookupError:
                    pass
            raise AcceptanceFailure("Claude cancellation tool process survived cleanup")
        artifact = workspace / "artifact.txt"
        if artifact.read_text(encoding="utf-8").strip() != "claude-workspace-ok":
            raise AcceptanceFailure("Claude workspace side effect is missing or incorrect")
        if (workspace / "seed.txt").exists():
            raise AcceptanceFailure("context seed survived the first Claude turn")
        for source, marker in markers.items():
            if marker.read_text(encoding="utf-8") != f"{source}\n":
                raise AcceptanceFailure(f"Claude ignored the {source} settings hook")
        if precedence_marker.read_text(encoding="utf-8") != "local\n":
            raise AcceptanceFailure("Claude settings precedence is incorrect")

        invalid_model = {
            **live_profile,
            "model": f"troupe-invalid-model-{secrets.token_hex(8)}",
        }
        _require_error(
            _run_case(
                provider="claude",
                mode="invalid-model",
                workspace=workspace,
                profile=invalid_model,
                seed_token=seed_token,
                user_settings=user_settings,
                user_settings_target=user_settings_target,
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
                provider="claude",
                mode="invalid-effort",
                workspace=workspace,
                profile=invalid_effort,
                seed_token=seed_token,
                user_settings=user_settings,
                user_settings_target=user_settings_target,
            ),
            error_type="AgentSessionStartError",
            code="configuration_invalid",
            phase="configure",
        )

        _require_error(
            _run_case(
                provider="claude",
                mode="auth-required",
                workspace=workspace,
                profile=live_profile,
                seed_token=seed_token,
                unlogged=True,
                user_settings=user_settings,
                user_settings_target=user_settings_target,
            ),
            error_type="AgentSessionBrokenError",
            code="authentication_lost",
            phase=None,
        )
    finally:
        current_user_settings = (
            user_settings_target.read_bytes() if user_settings_target.is_file() else None
        )
        if current_user_settings != original_user_settings:
            raise AcceptanceFailure("live isolation modified real Claude user settings")
        if ownership_marker.read_text(encoding="ascii") != "claude\n":
            raise AcceptanceFailure("refusing to clean an unowned live workspace")
        shutil.rmtree(workspace)
    if workspace.exists():
        raise AcceptanceFailure("live Claude workspace survived cleanup")


def main(argv: list[str]) -> int:
    if len(argv) != 1 or argv[0] not in PRODUCTIONS:
        raise AcceptanceFailure("usage: provider_acceptance.py {codex|claude}")
    from troupe import _runtime

    if hasattr(_runtime, "_agent_test_reset_launch"):
        raise AcceptanceFailure("live acceptance requires a build without agent-test-support")
    provider = argv[0]
    base, profile = _load_profile(provider)
    if provider == "codex":
        _run_codex(base, profile)
    else:
        _run_claude(base, profile)
    return 0


if __name__ == "__main__":
    try:
        raise SystemExit(main(sys.argv[1:]))
    except AcceptanceFailure as error:
        print(str(error), file=sys.stderr)
        raise SystemExit(1) from None
