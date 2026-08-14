from __future__ import annotations

import os
import re
import shutil
import signal
import subprocess
import sys
import tempfile
from pathlib import Path

from artifact_layout import ArtifactLayoutError, GateDescriptor, load_gate_descriptors


ENV_REFERENCE = re.compile(r"\$\{(?P<name>[A-Z][A-Z0-9_]*):\?\}")
CACHE_ENV = {
    "npm": "TROUPE_NPM_CACHE",
    "perfetto": "TROUPE_PERFETTO_CACHE",
    "playwright": "TROUPE_PLAYWRIGHT_CACHE",
}


class BootstrapGateError(RuntimeError):
    pass


def _is_within(path: Path, parent: Path) -> bool:
    try:
        path.relative_to(parent)
    except ValueError:
        return False
    return True


def _validated_temp_parent(repository_root: Path, environ: dict[str, str]) -> tuple[Path | None, bool]:
    raw = environ.get("TROUPE_GATE_TMP")
    if raw is None:
        return None, True
    parent = Path(raw)
    try:
        resolved = parent.resolve(strict=True)
    except OSError as error:
        raise BootstrapGateError("TROUPE_GATE_TMP must be an existing directory") from error
    if not resolved.is_dir() or _is_within(resolved, repository_root):
        raise BootstrapGateError("TROUPE_GATE_TMP must be a repository-external directory")
    return resolved, False


def _gate_environment(
    descriptor: GateDescriptor,
    temporary: Path,
    caller: dict[str, str],
) -> dict[str, str]:
    environment = dict(caller)
    caller_home = caller.get("HOME")
    if "RUSTUP_HOME" not in environment and caller_home:
        rustup_home = Path(caller_home) / ".rustup"
        if rustup_home.is_dir():
            environment["RUSTUP_HOME"] = str(rustup_home.resolve())
    for unsafe in ("CONDA_PREFIX", "PYTHONHOME", "VIRTUAL_ENV", "UV_PROJECT_ENVIRONMENT"):
        environment.pop(unsafe, None)
    environment.update(
        {
            "HOME": str(temporary / "home"),
            "TMPDIR": str(temporary / "tmp"),
            "UV_CACHE_DIR": str(temporary / "uv-cache"),
            "UV_PROJECT_ENVIRONMENT": str(temporary / "venv"),
            "PYTHONDONTWRITEBYTECODE": "1",
            "PYTEST_ADDOPTS": "-p no:cacheprovider",
            "TROUPE_DIAGNOSTIC_BOOTSTRAP_GATE": "1",
        }
    )
    for directory in ("home", "tmp", "uv-cache"):
        (temporary / directory).mkdir()

    for name, policy in descriptor.env.items():
        if policy == "required":
            if not caller.get(name):
                raise BootstrapGateError(f"required gate environment is missing: {name}")
            environment[name] = caller[name]
        elif policy == "optional":
            if name in caller:
                environment[name] = caller[name]
            else:
                environment.pop(name, None)
        else:
            environment[name] = policy
    for cache in descriptor.cache_requirements:
        name = CACHE_ENV[cache]
        if not caller.get(name):
            raise BootstrapGateError(f"required gate cache is missing: {name}")
        environment[name] = caller[name]
    return environment


def _expand_argument(argument: str, environment: dict[str, str]) -> str:
    def replace(match: re.Match[str]) -> str:
        name = match.group("name")
        value = environment.get(name)
        if not value:
            raise BootstrapGateError(f"gate argument requires missing environment: {name}")
        return value

    expanded = ENV_REFERENCE.sub(replace, argument)
    if "${" in expanded:
        raise BootstrapGateError(f"gate argument contains an unsupported environment reference: {argument}")
    return expanded


def _reject_native_command(command: tuple[str, ...]) -> None:
    executable = Path(command[0]).name
    if executable == "troupe":
        raise BootstrapGateError("bootstrap gates must not invoke the troupe console script")
    if executable in {"python", "python3"}:
        if "-c" in command:
            raise BootstrapGateError("bootstrap gates must not execute inline Python")
        if len(command) >= 3 and command[1] == "-m" and command[2] in {"troupe", "troupe._runtime"}:
            raise BootstrapGateError("bootstrap gates must not import the native runtime")
    if any("troupe._runtime" in argument for argument in command):
        raise BootstrapGateError("bootstrap gates must not reference the native runtime")


def _resolve_executable(command: list[str], temporary: Path) -> list[str]:
    executable = Path(command[0]).name
    if executable in {"pytest", "python", "python3"}:
        candidate = temporary / "venv/bin" / ("python" if executable.startswith("python") else "pytest")
        command[0] = str(candidate)
    return command


def run_bootstrap_gate(
    repository_root: Path,
    node_id: str,
    *,
    environ: dict[str, str] | None = None,
) -> None:
    root = repository_root.resolve(strict=True)
    caller = dict(os.environ if environ is None else environ)
    try:
        descriptors = load_gate_descriptors(root)
    except ArtifactLayoutError as error:
        raise BootstrapGateError(str(error)) from error
    if node_id not in descriptors:
        raise BootstrapGateError(f"unknown diagnostic node: {node_id}")
    descriptor = descriptors[node_id]
    if descriptor.state != "realized":
        raise BootstrapGateError(f"diagnostic node gate is not realized: {node_id}")

    parent, remove_parent = _validated_temp_parent(root, caller)
    temporary = Path(tempfile.mkdtemp(prefix=f"troupe-{node_id.lower()}-", dir=parent)).resolve()
    if _is_within(temporary, root):
        shutil.rmtree(temporary, ignore_errors=True)
        raise BootstrapGateError("bootstrap gate temporary directory is inside the repository")

    previous_handlers: dict[int, signal.Handlers] = {}

    def interrupted(signum: int, _frame: object) -> None:
        raise SystemExit(128 + signum)

    try:
        for signum in (signal.SIGINT, signal.SIGTERM):
            previous_handlers[signum] = signal.signal(signum, interrupted)
        environment = _gate_environment(descriptor, temporary, caller)
        subprocess.run(
            ["uv", "sync", "--frozen", "--all-groups", "--no-install-project"],
            cwd=root,
            env=environment,
            check=True,
        )
        environment["PATH"] = f"{temporary / 'venv/bin'}{os.pathsep}{environment.get('PATH', '')}"
        for structured in descriptor.argv:
            _reject_native_command(structured)
            command = [_expand_argument(argument, environment) for argument in structured]
            subprocess.run(
                _resolve_executable(command, temporary),
                cwd=root,
                env=environment,
                check=True,
            )
    except subprocess.CalledProcessError as error:
        raise BootstrapGateError(
            f"diagnostic bootstrap gate {node_id} failed with exit code {error.returncode}"
        ) from error
    finally:
        for signum, handler in previous_handlers.items():
            signal.signal(signum, handler)
        shutil.rmtree(temporary, ignore_errors=False)
        if remove_parent and parent is not None:
            shutil.rmtree(parent, ignore_errors=False)


def main(argv: list[str] | None = None) -> int:
    arguments = sys.argv[1:] if argv is None else argv
    if len(arguments) != 1:
        print("usage: diagnostic_bootstrap_gate.py <node-id>", file=sys.stderr)
        return 2
    repository_root = Path(__file__).resolve().parents[2]
    try:
        run_bootstrap_gate(repository_root, arguments[0])
    except (BootstrapGateError, OSError) as error:
        print(f"diagnostic bootstrap gate: {error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
