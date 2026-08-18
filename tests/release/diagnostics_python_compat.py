#!/usr/bin/env python3
"""Validate one installed Troupe wheel on one CPython version."""

from __future__ import annotations

import argparse
import hashlib
import importlib
import importlib.machinery
import json
import platform
import shutil
import sys
import zipfile
from decimal import Decimal
from pathlib import Path
from typing import Any, cast

import diagnostics_wheel_smoke


PUBLIC_EXPORTS = [
    "Actor",
    "ActorHandle",
    "AgentAuthenticationRequiredError",
    "AgentError",
    "AgentProfile",
    "AgentResultError",
    "AgentResultIssue",
    "AgentResultMissingError",
    "AgentSessionBrokenError",
    "AgentSessionBusyError",
    "AgentSessionError",
    "AgentSessionStartError",
    "AgentTurnError",
    "Cue",
    "CueContextError",
    "Effect",
    "EffectContextError",
    "Production",
    "act_schema",
    "diagnostics",
]
DIAGNOSTIC_EXPORTS = [
    "ActTokenUsageFinalized",
    "ActorDetail",
    "AffectedElapsedInterval",
    "AgentMessageCompleted",
    "AgentMessageDelta",
    "AgentPlanSnapshot",
    "AgentSessionBrokenDetail",
    "AgentSessionDetail",
    "AgentTurnTerminalDetail",
    "CausalLink",
    "ContextUsageSampled",
    "CounterSampled",
    "CustomCounterSampled",
    "CustomInstantOccurred",
    "CustomSpanFinished",
    "CustomSpanStarted",
    "DiagnosticAttributeValue",
    "DiagnosticAttributes",
    "DiagnosticCallbackFailure",
    "DiagnosticCapture",
    "DiagnosticComponentFailedDetail",
    "DiagnosticContextError",
    "DiagnosticDimension",
    "DiagnosticDimensions",
    "DiagnosticDropCount",
    "DiagnosticEvent",
    "DiagnosticScalar",
    "DiagnosticScope",
    "DiagnosticSink",
    "DiagnosticSinkStateError",
    "DiagnosticSinkSummary",
    "DiagnosticToolInput",
    "DiagnosticToolLocation",
    "DiagnosticToolOutput",
    "EffectDetail",
    "EmptyDetail",
    "FrozenJsonArray",
    "FrozenJsonObject",
    "FrozenJsonValue",
    "InstantDetail",
    "InstantOccurred",
    "ObservationGap",
    "PlanEntry",
    "ProductionConstructDetail",
    "ProductionLoadDetail",
    "ProductionPathResolutionDetail",
    "ResultIssue",
    "ResultTransitionDetail",
    "SpanFinished",
    "SpanStartDetail",
    "SpanStarted",
    "ToolCallDetail",
    "counter",
    "event",
    "span",
]
PACKAGE_MEMBERS = {
    "__init__.py",
    "__init__.pyi",
    "act_schema.pyi",
    "diagnostics.pyi",
    "py.typed",
}


class CompatibilityError(RuntimeError):
    pass


def require(condition: bool, message: str) -> None:
    if not condition:
        raise CompatibilityError(message)


def sha256_file(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as stream:
        while chunk := stream.read(1024 * 1024):
            digest.update(chunk)
    return digest.hexdigest()


def installed_file(value: object, label: str, environment: Path) -> Path:
    if not isinstance(value, str):
        raise CompatibilityError(f"{label} has no file path")
    path = Path(value).resolve(strict=True)
    try:
        path.relative_to(environment)
    except ValueError as error:
        raise CompatibilityError(f"{label} was imported outside the venv") from error
    require(path.is_file(), f"{label} is not a regular file")
    return path


def validate_wheel_install(
    wheel: Path,
    expected_sha256: str,
    package: Path,
) -> tuple[str, int]:
    require(wheel.is_absolute(), "wheel path must be absolute")
    require(not wheel.is_symlink() and wheel.is_file(), "wheel is not a regular file")
    require(sha256_file(wheel) == expected_sha256, "wheel SHA-256 drifted")
    try:
        with zipfile.ZipFile(wheel) as archive:
            names = [
                info.filename
                for info in archive.infolist()
                if not info.is_dir() and info.filename.startswith("troupe/")
            ]
            require(
                len(names) == len(set(names)), "wheel has duplicate package members"
            )
            relative = {name.removeprefix("troupe/") for name in names}
            native = [
                name
                for name in relative
                if name.endswith(".so") and name.startswith("_runtime")
            ]
            require(len(native) == 1, "wheel does not have one native module")
            require(
                relative == {*PACKAGE_MEMBERS, native[0]},
                "wheel package inventory drifted",
            )
            for name in relative:
                installed = package / name
                require(
                    installed.is_file() and not installed.is_symlink(),
                    f"installed wheel member is unavailable: {name}",
                )
                require(
                    installed.read_bytes() == archive.read(f"troupe/{name}"),
                    f"installed wheel member differs: {name}",
                )
            native_data = archive.read(f"troupe/{native[0]}")
    except (OSError, KeyError, zipfile.BadZipFile) as error:
        raise CompatibilityError(
            f"could not validate wheel install: {error}"
        ) from error
    return hashlib.sha256(native_data).hexdigest(), len(native_data)


def validate_public_api() -> tuple[Path, str, int]:
    troupe = importlib.import_module("troupe")
    runtime = importlib.import_module("troupe._runtime")
    diagnostics = importlib.import_module("troupe.diagnostics")
    environment = Path(sys.prefix).resolve(strict=True)
    executable = Path(sys.executable).absolute()
    try:
        executable.relative_to(environment)
    except ValueError as error:
        raise CompatibilityError("probe Python is outside its venv") from error
    troupe_file = installed_file(troupe.__file__, "troupe", environment)
    runtime_file = installed_file(runtime.__file__, "troupe._runtime", environment)
    require(
        any(
            str(runtime_file).endswith(suffix)
            for suffix in importlib.machinery.EXTENSION_SUFFIXES
        ),
        "runtime is not a native extension",
    )
    console = shutil.which("troupe")
    installed_file(console, "troupe console", environment)
    require(troupe.__all__ == PUBLIC_EXPORTS, "troupe public exports drifted")
    require(diagnostics.__all__ == DIAGNOSTIC_EXPORTS, "diagnostics exports drifted")
    require(
        diagnostics.__name__ == "troupe.diagnostics"
        and sys.modules.get("troupe.diagnostics") is diagnostics,
        "diagnostics module registration drifted",
    )
    require(troupe.diagnostics is diagnostics, "diagnostics module identity drifted")
    require(runtime.diagnostics is diagnostics, "native diagnostics identity drifted")
    require(troupe.Production is runtime.Production, "Production identity drifted")

    package = troupe_file.parent
    stub = (package / "__init__.pyi").read_text(encoding="utf-8")
    diagnostics_stub = (package / "diagnostics.pyi").read_text(encoding="utf-8")
    require(
        "from . import diagnostics as diagnostics" in stub,
        "top-level diagnostics stub export is missing",
    )
    require(
        "diagnostic_sink: diagnostics.DiagnosticSink | None = None" in stub,
        "Actor.act diagnostic sink stub is missing",
    )
    require(
        "class DiagnosticSink(_ABC):" in diagnostics_stub
        and "ViewSpec" not in diagnostics_stub
        and "diagnostic_views" not in diagnostics_stub,
        "diagnostics stub surface drifted",
    )
    require((package / "py.typed").read_bytes() == b"", "py.typed marker drifted")
    return package, sha256_file(runtime_file), runtime_file.stat().st_size


def validate_python_extensions() -> dict[str, object]:
    from troupe import diagnostics  # type: ignore[import-not-found]

    class ProbeSink(diagnostics.DiagnosticSink):
        def on_event(self, event: diagnostics.DiagnosticEvent, /) -> None:
            self.last_event = event

    sink = ProbeSink(
        capture=diagnostics.DiagnosticCapture(
            tool_inputs=True,
            tool_outputs=True,
        )
    )
    require(sink.state == "UNBOUND", "fresh sink state drifted")
    require(
        sink.capture.tool_inputs and sink.capture.tool_outputs,
        "sink capture options drifted",
    )

    custom_calls = (
        lambda: diagnostics.event(
            "compat.event",
            attributes={"version": platform.python_version()},
        ),
        lambda: diagnostics.counter(
            "compat.counter",
            Decimal("1.5"),
            unit="items",
            dimensions={"runtime": "cpython"},
        ),
        lambda: diagnostics.span("compat.span"),
    )
    for call in custom_calls:
        try:
            value = call()
            if value is not None:
                with value:
                    pass
        except diagnostics.DiagnosticContextError:
            continue
        raise CompatibilityError("custom diagnostics escaped the Runtime context gate")

    return {
        "sink": True,
        "custom": True,
        "view_renderers": [],
    }


def exercise(
    workspace: Path,
    wheel: Path,
    expected_python: str,
    expected_wheel_sha256: str,
) -> dict[str, Any]:
    actual_python = f"{sys.version_info.major}.{sys.version_info.minor}"
    require(platform.python_implementation() == "CPython", "interpreter is not CPython")
    require(actual_python == expected_python, "interpreter version drifted")
    require(workspace.is_dir(), "compatibility workspace is unavailable")
    package, installed_native_sha256, installed_native_bytes = validate_public_api()
    wheel_native_sha256, wheel_native_bytes = validate_wheel_install(
        wheel,
        expected_wheel_sha256,
        package,
    )
    require(
        (installed_native_sha256, installed_native_bytes)
        == (wheel_native_sha256, wheel_native_bytes),
        "installed native module differs from the wheel",
    )
    extensions = validate_python_extensions()
    runtime_workspace = workspace / "runtime"
    runtime_workspace.mkdir()
    runtime = diagnostics_wheel_smoke.exercise(
        runtime_workspace,
        ("active", "archive"),
    )
    require(runtime["production_imports"] == 1, "archive smoke re-imported Production")
    installed = cast(dict[str, Any], runtime["installed"])
    active = cast(dict[str, Any], runtime["active"])
    archive = cast(dict[str, Any], runtime["archive"])
    require(
        installed["native_sha256"] == installed_native_sha256,
        "runtime smoke loaded another native module",
    )
    return {
        "schema": "troupe.diagnostics.python-compat-probe.v1",
        "python": actual_python,
        "implementation": platform.python_implementation(),
        "wheel_sha256": expected_wheel_sha256,
        "native_sha256": installed_native_sha256,
        "native_bytes": installed_native_bytes,
        "package_members": sorted(PACKAGE_MEMBERS),
        "extensions": extensions,
        "runtime": {
            "active": active["status"],
            "archive": archive["status"],
            "trace_bytes": archive["trace_bytes"],
            "production_imports": runtime["production_imports"],
        },
    }


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--workspace", type=Path, required=True)
    parser.add_argument("--wheel", type=Path, required=True)
    parser.add_argument("--expected-python", required=True)
    parser.add_argument("--expected-wheel-sha256", required=True)
    arguments = parser.parse_args()
    try:
        result = exercise(
            arguments.workspace.resolve(strict=True),
            arguments.wheel.resolve(strict=True),
            arguments.expected_python,
            arguments.expected_wheel_sha256,
        )
    except (
        CompatibilityError,
        diagnostics_wheel_smoke.SmokeError,
        OSError,
        ValueError,
        KeyError,
        TypeError,
        json.JSONDecodeError,
    ) as error:
        print(f"troupe Python compatibility failed: {error}", file=sys.stderr)
        return 1
    print(json.dumps(result, separators=(",", ":")))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
