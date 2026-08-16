from __future__ import annotations

from typing import TypeAlias

from troupe import Actor, act_schema, diagnostics


JsonValue: TypeAlias = (
    None | bool | int | float | str | list["JsonValue"] | dict[str, "JsonValue"]
)


class EvaluationSink(diagnostics.DiagnosticSink):
    def __init__(self) -> None:
        super().__init__(
            capture=diagnostics.DiagnosticCapture(
                tool_inputs=True,
                tool_outputs=True,
            )
        )
        self.message_text: list[str] = []
        self.final_usage: diagnostics.ActTokenUsageFinalized | None = None

    def on_event(self, event: diagnostics.DiagnosticEvent, /) -> None:
        if isinstance(event, diagnostics.AgentMessageDelta):
            self.message_text.append(event.text_delta)
        elif isinstance(event, diagnostics.ActTokenUsageFinalized):
            self.final_usage = event


async def run_evaluated_act(
    actor: Actor,
) -> tuple[dict[str, JsonValue], diagnostics.DiagnosticSinkSummary]:
    sink = EvaluationSink()
    result = await actor.act(
        script="Inspect the repository and return whether it is ready.",
        output_schema={
            "ready": act_schema.BoolValue(description="whether the repository is ready")
        },
        diagnostic_sink=sink,
    )
    summary = await sink.wait_closed()
    return result, summary


def main() -> None:
    sink = EvaluationSink()
    assert sink.state == "UNBOUND"
    assert sink.capture.tool_inputs
    assert sink.capture.tool_outputs


if __name__ == "__main__":
    main()
