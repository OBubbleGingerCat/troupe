from examples.diagnostics.custom import record_batch
from examples.diagnostics.sink import JsonValue, run_evaluated_act
from examples.diagnostics.views import DIAGNOSTIC_VIEWS, ObservedProduction
from troupe import Actor, diagnostics
from typing_extensions import assert_type


async def check_sink_example(actor: Actor) -> None:
    result, summary = await run_evaluated_act(actor)
    assert_type(result, dict[str, JsonValue])
    assert_type(summary, diagnostics.DiagnosticSinkSummary)


def check_custom_and_view_examples() -> None:
    assert_type(record_batch(queue_depth=3, region="east"), None)
    assert_type(DIAGNOSTIC_VIEWS, tuple[diagnostics.ViewSpec, ...])
    assert_type(ObservedProduction.diagnostic_views, tuple[diagnostics.ViewSpec, ...])
