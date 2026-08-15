from troupe import Actor, act_schema


async def invalid_actor_act_sinks(actor: Actor) -> None:
    schema: dict[str, act_schema.FieldSpec] = {
        "value": act_schema.Int64Value(description="typed value")
    }
    await actor.act(
        script="Reject an arbitrary object.",
        output_schema=schema,
        diagnostic_sink=object(),  # E: arg-type
    )
    await actor.act(
        script="Reject a string.",
        output_schema=schema,
        diagnostic_sink="sink",  # E: arg-type
    )
