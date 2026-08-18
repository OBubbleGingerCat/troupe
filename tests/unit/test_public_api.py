from __future__ import annotations

import asyncio
import importlib
import inspect
import sys
from types import ModuleType

import pytest


DIAGNOSTIC_PUBLIC = [
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


def _modules() -> tuple[ModuleType, ModuleType]:
    troupe = importlib.import_module("troupe")
    runtime = importlib.import_module("troupe._runtime")
    return troupe, runtime


def test_public_symbols_have_their_declared_implementation_boundary() -> None:
    troupe, runtime = _modules()

    expected = [
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
    assert troupe.__all__ == expected
    assert {name for name in vars(troupe) if not name.startswith("_")} == set(expected)
    native = [
        name
        for name in expected
        if name not in {"AgentProfile", "act_schema", "diagnostics"}
    ]
    for name in native:
        public = getattr(troupe, name)
        assert public is getattr(runtime, name)
        assert public.__module__ == "troupe"
    assert not hasattr(runtime, "AgentProfile")
    assert troupe.AgentProfile.__module__ == "troupe"
    assert troupe.act_schema is runtime.act_schema
    assert troupe.act_schema.__name__ == "troupe.act_schema"
    assert sys.modules["troupe.act_schema"] is troupe.act_schema
    assert troupe.diagnostics is runtime.diagnostics
    assert troupe.diagnostics.__name__ == "troupe.diagnostics"
    assert sys.modules["troupe.diagnostics"] is troupe.diagnostics


def test_diagnostics_module_has_exact_native_surface() -> None:
    troupe, _ = _modules()
    diagnostics = troupe.diagnostics

    assert diagnostics.__all__ == DIAGNOSTIC_PUBLIC
    assert {
        name for name in vars(diagnostics) if not name.startswith("_")
    } == set(DIAGNOSTIC_PUBLIC)
    for name in DIAGNOSTIC_PUBLIC:
        value = getattr(diagnostics, name)
        if type(value) is type or inspect.isfunction(value):
            assert value.__module__ == "troupe.diagnostics"

    assert not any(name.endswith("V1") for name in DIAGNOSTIC_PUBLIC)
    assert not hasattr(diagnostics, "ActDiagnosticEvent")
    assert not hasattr(diagnostics, "compile")


def test_actor_and_effect_are_subclassable_but_handle_and_cue_are_final() -> None:
    troupe, _ = _modules()

    class CustomActor(troupe.Actor):
        pass

    class CustomEffect(troupe.Effect):
        pass

    class CustomCueContextError(troupe.CueContextError):
        pass

    class CustomEffectContextError(troupe.EffectContextError):
        pass

    assert issubclass(CustomActor, troupe.Actor)
    assert issubclass(CustomEffect, troupe.Effect)
    assert issubclass(CustomCueContextError, RuntimeError)
    assert issubclass(CustomEffectContextError, RuntimeError)

    with pytest.raises(TypeError):
        type("CustomHandle", (troupe.ActorHandle,), {})
    with pytest.raises(TypeError):
        type("CustomCue", (troupe.Cue,), {})


def test_constructor_accepts_only_one_positional_list_of_strings() -> None:
    troupe, _ = _modules()
    production_type = troupe.Production

    assert isinstance(production_type([]), production_type)
    assert isinstance(production_type(["--value", "1"]), production_type)
    assert isinstance(production_type(["\udcff"]), production_type)

    with pytest.raises(TypeError):
        production_type()
    with pytest.raises(TypeError):
        production_type([], [])
    with pytest.raises(TypeError):
        production_type(args=[])

    for invalid in ((), "value", None, 1):
        with pytest.raises(TypeError):
            production_type(invalid)

    for invalid in ([1], [None], ["valid", object()]):
        with pytest.raises(TypeError):
            production_type(invalid)


def test_base_class_does_not_retain_args() -> None:
    troupe, _ = _modules()
    value = "".join(["not", "-interned"])
    args = [value]
    args_references = sys.getrefcount(args)
    value_references = sys.getrefcount(value)

    production = troupe.Production(args)

    assert not hasattr(production, "args")
    assert sys.getrefcount(args) == args_references
    assert sys.getrefcount(value) == value_references


def test_default_hooks_return_awaitables_with_exact_results() -> None:
    troupe, _ = _modules()
    production = troupe.Production([])

    async def exercise() -> None:
        start = production.start()
        scene = production.scene()
        stop = production.stop()

        assert inspect.isawaitable(start)
        assert inspect.isawaitable(scene)
        assert inspect.isawaitable(stop)
        assert await start is None
        with pytest.raises(
            NotImplementedError,
            match=r"^Production\.scene\(\) is not implemented$",
        ):
            await scene
        assert await stop is None

    asyncio.run(exercise())


def test_python_subclass_owns_init_and_overrides_scene() -> None:
    troupe, _ = _modules()
    events: list[str] = []
    init_calls: list[list[str]] = []

    class CustomProduction(troupe.Production):
        def __init__(self, args: list[str]) -> None:
            init_calls.append(args)
            self.received = args

        async def scene(self) -> None:
            events.append("scene")

    args = ["--value", "1"]
    production = CustomProduction(args)

    assert type(production) is CustomProduction
    assert init_calls == [args]
    assert init_calls[0] is args
    assert production.received is args

    with pytest.raises(TypeError):
        CustomProduction(())  # type: ignore[arg-type]
    assert init_calls == [args]

    async def exercise() -> None:
        assert await production.start() is None
        assert await production.scene() is None
        assert await production.stop() is None

    asyncio.run(exercise())
    assert events == ["scene"]
