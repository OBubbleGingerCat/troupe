from troupe import Actor, act_schema, diagnostics


class EvaluationSink(diagnostics.DiagnosticSink):
    def on_event(self, event: diagnostics.DiagnosticEvent, /) -> None:
        _ = event


async def accepted_actor_act_calls(
    actor: Actor,
    sink: EvaluationSink,
) -> None:
    schema: dict[str, act_schema.FieldSpec] = {
        "value": act_schema.Int64Value(description="typed value")
    }
    with_sink = await actor.act(
        script="Return a typed value.",
        output_schema=schema,
        diagnostic_sink=sink,
    )
    default_none = await actor.act(
        script="Return another typed value.",
        output_schema=schema,
    )
    explicit_none = await actor.act(
        script="Return one more typed value.",
        output_schema=schema,
        diagnostic_sink=None,
    )
    _ = (with_sink, default_none, explicit_none)
