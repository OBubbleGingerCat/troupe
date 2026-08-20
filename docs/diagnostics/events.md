# Diagnostic events

`DiagnosticEvent` is the canonical fact model shared by the persistent store,
HTTP and SSE APIs, the Web interface, CLI output, Perfetto export, and the
optional per-Act Python sink. Consumer-specific projections do not create a
second event history.

## Envelope and ordering

Every event has the same immutable envelope:

- `schema_version` identifies the event schema.
- `run_id` is the canonical UUID of one Production Run.
- `sequence` is the global, strictly increasing Run sequence and starts at 1.
- `elapsed_ns` is time relative to the start of that Run.
- `scope` contains the optional `scene_id`, `actor_id`, `cue_id`, `effect_id`,
  `act_id`, `tool_call_id`, and `session_generation` dimensions.
- `caused_by` carries typed causal links to earlier source sequences.

Committed storage is a dense prefix `1..W`, where `W` is the committed
watermark. A snapshot and its materialized spans, messages, counters, and usage
are bound to one read-model watermark. Values declared as `u64`, including
sequence and elapsed time, use canonical decimal strings on JSON wires so a
browser never loses integer precision.

A filtered consumer can legitimately see sequence numbers that are not
adjacent because events for other scopes or kinds were omitted. That alone is
not evidence loss.

## Spans, instants, and counters

A span start records its kind, scope, detail, and optional parent. Its start
sequence is the stable `span_id`; the matching finish refers to that ID and
records an outcome and optional stable error code. An absent finish means the
captured prefix contains an open span. Readers never invent an end time.

Instants mark point-in-time transitions such as Cue admission, tool updates, or
result validation. Counters are absolute samples rather than deltas. Scope and
causal links keep concurrently executing Scenes, Actors, Cues, Acts, tools, and
Effects distinct even when timestamps are equal.

Agent message deltas are append-only facts keyed by stable message identity.
Context occupancy samples describe the persistent session at a moment in time;
final Act token accounting describes one complete Act. Neither should be
derived from the other.

## Observation gaps

`ObservationGap` is a canonical producer-side fact that some source
observations are known to be missing. It names the producer and reason and may
bound the affected count, event kind, scope, and elapsed interval. Unknown
count remains unknown rather than becoming zero.

The runtime's `cue-diagnostics-suppressed` gap is emitted after durable-ingress
pressure causes whole Cue captures to be omitted. Its `dropped_count` counts
suppressed Cues, its elapsed interval spans the first suppression through
capture recovery (or Run finalization), and null affected kind/scope means the
omission can include every diagnostic kind and scope produced by those Cues.

Subscriber overflow, reconnect, and replay controls are delivery state, not
Production facts. SSE control frames therefore consume no Run sequence and are
not stored, delivered to a Python sink, or exported. A consumer-local loss must
be reported by that consumer and must not be rewritten as a global
`ObservationGap`.

An incomplete archive also does not fabricate a gap for an unobserved crash
tail. Its durable `clean_shutdown=false` metadata is the evidence that the
committed prefix might not cover everything that happened before process loss.

## Completeness

The store preserves every successfully committed canonical event for the life
of the Run archive. Individual consumers remain bounded and may report a local
gap, truncation, stale query, or unavailable value. Evaluate completeness using
the Run watermark and lifecycle metadata together with explicit
`ObservationGap` and consumer-local delivery status; do not infer it from a
quiet timeline or a missing optional value.
