# Live Web diagnostics

The Web interface is the real-time observation surface for active Productions
and the same read-only inspection surface for inactive archives. An active
Runtime serves the embedded interface, HTTP queries, and event stream from one
origin. `troupe diagnostic serve` supplies that same embedded interface for an
inactive archive on loopback. The archive contains canonical diagnostics, not a
copy of the frontend.

## Execution hierarchy

The execution tree and timeline preserve Production, Scene, Actor, Cue, Act,
and tool identity. An Actor row is a logical group and status summary. Each Cue
under that Actor keeps its own mailbox wait, execution, Acts, tools, result, and
outcome:

```text
Production
`-- Scene 0042
    `-- Actor investigator        1 done / 1 running / 1 queued
        |-- Cue c-102             completed
        |   |-- mailbox wait
        |   |-- Actor.cued()
        |   `-- Act #1 / tools / result
        |-- Cue c-103             running
        |   `-- Act #2 / tools / result
        `-- Cue c-104             queued
            `-- mailbox wait ... now
```

Multiple Cues for one Actor are never merged onto an identity-free Actor track.
The Actor summary locates work; the Cue rows explain serialization and overlap.
Collapsing a Cue hides its children but retains the Cue's wait, execution, and
outcome. Different Actors can execute concurrently.

## Panels and selection

The primary workspace has Timeline, Agent, Events, Usage, and Views panels that
share one scope and time selection.

- Timeline is the hierarchical trace. It supports pan, zoom, live follow,
  open/completed spans, causal flows, gaps, counters, and selection.
- Agent is the transcript for the selected Actor, Cue, and Act. Stable message
  IDs assemble live message deltas; tool activity and result submission,
  rejection, repair, acceptance, or absence appear inline in sequence order.
  Thinking exposes activity and duration, never private thought content.
- Events is the bounded canonical event table plus typed inspector and filters.
- Usage separates Live context occupancy from Final Act accounting. Unknown,
  partial, and unavailable provider usage stay explicit, and aggregates show
  reported/finalized coverage instead of substituting zero.
- Views renders the Production's static Timeline, Metric, Table, and TimeSeries
  declarations. Queries execute on the server at a captured watermark; the
  browser does not execute Production Python or arbitrary renderer code.

Selecting a tree row, timeline item, transcript message, tool, result, or event
updates the common selection. Concurrent agent text remains grouped by
Actor/Cue/Act rather than being concatenated into one transcript.

## Live replay, reconnect, and pause

Bootstrap obtains a committed snapshot at watermark `W` and a bounded canonical
suffix through the same `W`, then starts an SSE stream strictly after it. The
server first sends `stream_ready`, replays committed events through the captured
head, and continues with live commits. Event IDs are canonical Run sequences.

Reconnect resumes from the last handled ID and deduplicates by
`(run_id, sequence)`. A delivery gap, invalid cursor, incompatible identity, or
unrecoverable replay requests a fresh status, snapshot, and suffix instead of
guessing missing state. `stream_closed` ends intentional live service. Only
committed events reach the page.

Pause freezes presentation, not ingestion or the Runtime. The toolbar shows the
number of unseen sequences while the client continues tracking the committed
watermark with a bounded live edge. Resume uses a server range query for data
that left the hot window, hydrates one consistent snapshot, and catches up. It
does not accumulate the whole paused raw stream in browser memory.

## Bounded browser state

The page does not mirror a whole long Run. The V1 release constants retain at
most 4,096 events in the visible window, four adjacent windows, and 256 live-edge
events. Derived collections are independently bounded, including 256 spans, 128
messages, 256 tool facts, 256 result facts, 128 context samples, 256 final Act
usage facts, 128 gaps, and 64 query results. Eviction marks projections for a
server refresh; it never silently claims the local projection is complete.

Canonical IDs, watermarks, cursors, token integers, and elapsed nanoseconds stay
as decimal strings or `bigint`. Only a viewport-relative, range-checked delta is
converted to a JavaScript number for pixels. Reload reconstructs state from the
server; diagnostic content exists only in the current page memory.

## Compatibility and content safety

Interactive V1 supports Chromium and Edge 111 or newer, Firefox 115 or newer,
and Safari 16.4 or newer, including their corresponding mobile engines. It
requires native `fetch`, `EventSource`, and `BigInt`. Missing browser capability
or a major event, API, stream-control, View, or UI schema mismatch produces an
explicit static compatibility surface before queries or live ingestion begin.

The server declares `security_scope="trusted_network"`. Pages use same-origin
relative routes and load no external script, font, or data. Captured strings are
rendered as text rather than HTML or executable markup. Content Security Policy,
`X-Content-Type-Options: nosniff`, `Referrer-Policy: no-referrer`, and
same-origin resource policy constrain the embedded interface. This content
safety does not make captured diagnostic data suitable for an untrusted
network.
