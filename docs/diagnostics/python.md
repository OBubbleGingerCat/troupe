# Python diagnostics API

The public Python diagnostics API is available as `troupe.diagnostics`. It has
three independent surfaces:

- an optional per-Act `DiagnosticSink` passed to `Actor.act()`;
- synchronous custom instrumentation with `event()`, `counter()`, and `span()`;
- static `ViewSpec` declarations on a `Production` class.

These surfaces observe or describe a Production. They do not replace the
mandatory Run diagnostics pipeline, and none of them can control an Act.

## Observe one Act

Subclass `DiagnosticSink`, call its base initializer, and implement one ordered
callback. The callback may be synchronous or asynchronous.

```python
from troupe import diagnostics


class EvaluationSink(diagnostics.DiagnosticSink):
    def __init__(self) -> None:
        super().__init__(
            capture=diagnostics.DiagnosticCapture(
                tool_inputs=True,
                tool_outputs=True,
            )
        )
        self.events: list[diagnostics.DiagnosticEvent] = []

    def on_event(self, event: diagnostics.DiagnosticEvent, /) -> None:
        self.events.append(event)
```

Pass a fresh instance through the keyword-only argument:

```python
sink = EvaluationSink()
result = await actor.act(
    script="Inspect the repository and return a decision.",
    output_schema=schema,
    diagnostic_sink=sink,
)
summary = await sink.wait_closed()
```

`Actor.act()` still returns its validated `dict`; diagnostics do not wrap or
replace that value. A callback failure is recorded in the sink summary and does
not change the Act result, agent session, or Production outcome.

The complete runnable pattern is in
[`examples/diagnostics/sink.py`](../../examples/diagnostics/sink.py).

### Event model

`DiagnosticEvent` is a closed union of immutable typed values. Every variant
has the same envelope: `schema_version`, `run_id`, global `sequence`, Run
relative `elapsed_ns`, a seven-field `DiagnosticScope`, and `caused_by` links.
Sequences delivered to one sink are strictly increasing. A sequence gap can
simply contain events for another part of the Production; only an
`ObservationGap` says that canonical source observations are known to be
incomplete.

The closed event-kind matrix is:

| Event class | Selected content |
|---|---|
| `SpanStarted` | Built-in span kind, typed start detail, and parent span |
| `SpanFinished` | Start sequence as `span_id`, outcome, and stable error code |
| `InstantOccurred` | Built-in instant kind, typed detail, and containing span |
| `CounterSampled` | Built-in absolute gauge sample |
| `AgentMessageDelta` | User-visible append-only text, never thought content |
| `AgentMessageCompleted` | Message byte/scalar counts and truncation status |
| `AgentPlanSnapshot` | One complete bounded plan snapshot |
| `ContextUsageSampled` | Session context occupancy and optional cumulative cost |
| `ActTokenUsageFinalized` | Final accounting for exactly one Act |
| `ObservationGap` | A canonical producer-side observation gap |
| `CustomSpanStarted` | A custom span name, attributes, and parent |
| `CustomSpanFinished` | A custom span ID and outcome |
| `CustomInstantOccurred` | A custom name, severity, attributes, and containing span |
| `CustomCounterSampled` | A custom absolute value, unit, and dimensions |

Provider raw payloads are not part of this hierarchy. Public event values own
their copied data and cannot be mutated by a callback.

### Capture policy

`DiagnosticCapture` is frozen, slotted, keyword-only, and fixed when the sink
binds. All eight fields require exact `bool` values:

| Field | Default | Sink projection |
|---|---:|---|
| `agent_messages` | `True` | Message delta/completion and thinking activity without thought content |
| `plans` | `True` | `AgentPlanSnapshot` |
| `tool_calls` | `True` | Tool span/update metadata |
| `result_validation` | `True` | Result transition metadata and rejection count |
| `usage` | `True` | Both context samples and final Act token accounting |
| `custom_events` | `True` | All four `Custom*` variants in this Act |
| `tool_inputs` | `False` | Optional `captured_input` on tool detail |
| `tool_outputs` | `False` | Optional `captured_output` on tool detail |

Tool input/output flags refine tool detail; they do not select tool events.
Either flag requires `tool_calls=True`.

Act/caller/turn lifecycle, Act admission/submission/cancellation/handoff, turn
activity/terminal/settled, Act-scoped `agent.turn.active`, cumulative
`diagnostic.dropped_events`, and relevant `ObservationGap` facts cannot be
disabled. Run, Cue, mailbox, and unrelated Act facts are never projected into a
per-Act sink. A sink-targeted component failure is written to the Run pipeline
but is not recursively delivered to that sink.

### Tool payload values

Tool input and output are explicit opt-ins for the requesting sink only. They
never add payload content to the canonical store, Web UI, CLI, or Perfetto
projection.

`DiagnosticToolInput`, `DiagnosticToolOutput`, and
`DiagnosticToolLocation` are final frozen values. The public container types
are `FrozenJsonArray` and `FrozenJsonObject`; the closed `FrozenJsonValue` union
contains `None`, `bool`, `int`, finite `Decimal`, `str`, and those immutable
containers. It intentionally exposes no mutable `dict`/`list` and no object
hook.

A requested direction that exceeds a snapshot or per-Act budget is represented
by its wrapper with `truncated=True`. `None` means the direction was not
selected or was absent, not that selected content was silently truncated.
Troupe validates structure and size but does not inspect keys, identify
credentials, redact, or rewrite opaque tool content. The caller enabling this
capture is responsible for its sensitivity.

## Sink lifecycle and completion

A sink instance can bind successfully to exactly one admitted Act. It cannot be
used concurrently or reused after completion. Its public lifecycle is:

```text
UNBOUND -> BOUND -> SEALED -> CLOSED
```

The base class rejects a missing `super().__init__()` as `uninitialized`,
`wait_closed()` before binding as `unbound`, and reuse as `already_bound`, all
through `DiagnosticSinkStateError.code`.

At Act terminal, Troupe admits final token usage first, then the
`act.lifecycle` finish, queues the selected terminal facts, expires the Act's
Python task authority, seals the sink, and drains already accepted callbacks.
Only then does the sink become `CLOSED`. This ordering lets an evaluator retain
the usage and lifecycle facts without giving its callback authority to call
`Actor.act()`, create an Effect, or publish Act-scoped custom events.

Callbacks run serially on Troupe's diagnostic thread, not on the Actor's event
loop and not on the agent hot path. They receive no Cue/Actor context. An async
callback may await ordinary async work, but it still cannot control or extend
the observed Act. The Web interface never executes sink callback Python.

`wait_closed()` has no timeout or force-close argument. Multiple calls and
multiple concurrent waiters receive the same immutable `DiagnosticSinkSummary`;
cancelling one waiter does not cancel sink delivery. The summary separates:

- Act outcome and close reason;
- delivered sequence range;
- subscriber-local dropped event/byte counts;
- producer-side source gaps and truncated payloads;
- callback failure or shutdown abandonment.

`summary.complete` means the requested evidence arrived without these delivery
losses. It does not mean that the Act succeeded, and a complete
`availability="unavailable"` usage event is still complete evidence.

## Context occupancy and Act token use

`ContextUsageSampled` and `ActTokenUsageFinalized` answer different questions.

`ContextUsageSampled.context_used_tokens/context_window_tokens` is a snapshot
of how much of the persistent agent session context is currently occupied. It
may span several Acts and may decrease after compaction. It is not cumulative
session usage and cannot be differenced to infer the current Act's cost.
Optional cumulative cost amount/currency belongs to that session snapshot.

Every started Act gets exactly one `ActTokenUsageFinalized` before its lifecycle
finish. It describes the whole agent turn, including tool loops and result
repair, when the provider supplies qualified final accounting. The six optional
fields are `provider_total_tokens`, `input_tokens`, `output_tokens`,
`thought_tokens`, `cached_read_tokens`, and `cached_write_tokens`.

- `available` requires provider total, input, and output; optional categories
  may be absent.
- `partial` carries at least one reported number but lacks a primary field.
- `unavailable` carries no numbers and a closed reason such as
  `prompt_not_submitted` or `usage_not_reported`.

Each present value is a non-negative Python `int`; zero is observed usage and
`None` is unknown. Troupe does not impose a public `u64` token maximum, infer a
missing total, assume categories are disjoint, or estimate tokens from context
occupancy. A thought-token count never exposes thought or reasoning content.
The delivery summary deliberately does not duplicate these values; evaluators
retain the immutable usage event they received.

## Custom instrumentation

The three publication calls are synchronous:

```python
diagnostics.event("example.batch_ready", attributes={"items": 12})
diagnostics.counter(
    "example.queue_depth",
    3,
    unit="items",
    dimensions={"region": "east"},
)
with diagnostics.span("example.process_batch", attributes={"region": "east"}):
    process()
```

They validate and copy caller input immediately. Event/counter calls and span
enter/exit each admit one canonical fact and return no canonical identity. Run,
sequence, time, scope, parent, and causality are Runtime-owned. A call requires
an active authorized Runtime task; invalid input or context raises before a
sequence is allocated. A custom span never suppresses the body exception and
records `completed`, `cancelled`, or `failed` on exit.

Names are 1-128 byte lowercase ASCII dotted identifiers with at least two
segments; `troupe.*` is reserved. Attribute keys are at most 64 UTF-8 bytes,
units at most 32 bytes, and one event may contain at most 32 attributes, 8
dimensions, 64 items per scalar list, and 64 KiB of caller-supplied canonical
payload.

Attributes accept flat `None`/`bool`/`int`/finite `float`/finite `Decimal`/`str`
values or lists/tuples of those scalars. Dimensions are single non-`None`
scalars. Counter values accept exact `int` (not `bool`), finite `float`, or
finite `Decimal` and are normalized exactly. There are no nested arbitrary
objects, custom serializers, or namespace registry. As with tool payload,
Troupe enforces shape and size but performs no content scan, credential-key
redaction, or rewriting.

See [`examples/diagnostics/custom.py`](../../examples/diagnostics/custom.py).

## Static diagnostic views

`ViewSpec` is the closed union of `TimelineView`, `MetricView`, `TableView`, and
`TimeSeriesView`. Each is a final frozen slotted keyword-only value paired with
its exact query type.

Declare an exact tuple on the Production class:

```python
class MyProduction(Production):
    diagnostic_views = (
        diagnostics.TimelineView(...),
        diagnostics.MetricView(...),
        diagnostics.TableView(...),
        diagnostics.TimeSeriesView(...),
    )
```

Selectors name one closed built-in kind or one custom dotted name. Queries may
use compatible severity/outcome filters, scalar attribute equality/existence,
one closed grouping dimension, and `count`, `sum`, `min`, `max`, `mean`, or
`latest` where valid for the source. A table declares 1-32 typed columns and a
page size from 1 through 500. Each view independently chooses `viewport` or
`run` time and `selection` or `run` scope.

SQL, regex, joins, arbitrary field paths, Python callables, and custom renderers
are not accepted. At startup, Runtime reads the class attribute statically,
requires exact built-in values and unique IDs, compiles at most 64 records, and
persists pure versioned JSON before calling the Production constructor. An
invalid active declaration therefore prevents constructor side effects. After
that point HTTP, live updates, the browser, and archive serving use persisted
records and do not import or execute Production Python.

All four declarations are executable in
[`examples/diagnostics/views.py`](../../examples/diagnostics/views.py).
