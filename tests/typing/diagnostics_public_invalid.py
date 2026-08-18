from decimal import Decimal

from troupe import diagnostics


class InvalidContextErrorSubclass(diagnostics.DiagnosticContextError):  # E: misc
    pass


class InvalidFrozenJsonSubclass(diagnostics.FrozenJsonArray):  # E: misc
    pass


class WrongSinkCallback(diagnostics.DiagnosticSink):
    def on_event(self, event: diagnostics.DiagnosticEvent, /) -> int:  # E: override
        _ = event
        return 1


def invalid_capture_types() -> None:
    diagnostics.DiagnosticCapture(tool_inputs=1)  # E: arg-type
    diagnostics.DiagnosticCapture(extra=True)  # E: call-arg


def mutable_tool_payload_containers() -> None:
    diagnostics.FrozenJsonArray(items=[1])  # E: arg-type
    diagnostics.FrozenJsonObject(entries={"value": 1})  # E: arg-type
    diagnostics.DiagnosticToolOutput(
        raw_output=None,
        content=[],  # E: arg-type
        locations=(),
        truncated=False,
    )


def wrong_optional_tool_payload() -> diagnostics.ToolCallDetail:
    output = diagnostics.DiagnosticToolOutput(
        raw_output=None,
        content=(),
        locations=(),
        truncated=False,
    )
    return diagnostics.ToolCallDetail(
        title="tool",
        tool_kind="read",
        status="completed",
        error_code=None,
        captured_input=output,  # E: arg-type
    )


def invalid_custom_calls() -> None:
    diagnostics.event("example.event", severity="fatal")  # E: arg-type
    diagnostics.counter("example.counter", object())  # E: arg-type
    diagnostics.span("example.span", attributes=[])  # E: arg-type


async def invalid_sink_commands(sink: diagnostics.DiagnosticSink) -> None:
    await sink.wait_closed(1)  # E: call-arg
    sink.close()  # E: attr-defined


def invalid_closed_literals() -> None:
    diagnostics.DiagnosticCapture(tool_inputs="yes")  # E: arg-type


def invalid_alias_values() -> None:
    frozen: diagnostics.FrozenJsonValue = 1.5  # E: assignment
    event: diagnostics.DiagnosticEvent = object()  # E: assignment
    dimension: diagnostics.DiagnosticDimension = []  # E: assignment
    _ = (frozen, event, dimension)


def frozen_capture_is_read_only(capture: diagnostics.DiagnosticCapture) -> None:
    capture.agent_messages = False  # E: misc


def decimal_is_valid_view_scalar_only() -> None:
    view_scalar: diagnostics.ViewScalar = Decimal("1.5")
    frozen_scalar: diagnostics.FrozenJsonValue = Decimal("1.5")
    _ = (view_scalar, frozen_scalar)
