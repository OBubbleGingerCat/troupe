# Live Web diagnostics

The Web interface is the real-time observation surface for active Productions
and the same read-only inspection surface for inactive archives. An active
Runtime serves the embedded interface, HTTP queries, and event stream from one
origin. `troupe diagnostic serve` supplies that same embedded interface for an
inactive archive on loopback. The archive contains canonical diagnostics, not a
copy of the frontend.

## Timeline and selection

The primary workspace is one system-owned Actor timeline. It contains Scene bands,
Actor lifelines, Cue send/wait/execute phases, Act bars, tool/message markers, and
Python custom spans/events. Hovering or selecting an item opens its typed details;
the browser never executes Production Python or accepts renderer registrations.

Every Actor lifetime uses one elapsed-time rail labeled `<name> Actor lifetime`.
The rail, creation boundary, and terminal/open boundary each expose their own hover
and keyboard-focus details. Multiple Cues keep their identities on the Actor lane;
their wait and execution bars explain serialization while different Actors may run
concurrently.

Live removes completed Actors after their lifetimes leave the rolling window. History
freezes the current committed watermark and validates the exact event prefix through
that watermark before enabling range selection or playback. Its Timeline projection
is not subject to the 256-span Live capacity, so completed temporary Actors and their
Python spans/events remain inspectable.

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

## Browser state

Live does not mirror a whole long Run. The V1 release constants retain at most
4,096 events in the visible window, four adjacent windows, and 4,096 live-edge events.
Derived Live collections are independently bounded, including 256 spans, 128 messages,
256 tool facts, 256 result facts, 128 context samples, 256 final Act usage facts,
128 gaps, and 64 query results. Eviction marks projections for a server refresh; it
never silently claims the local projection is complete.

Entering History is an explicit exception: V1 transfers the frozen canonical event
prefix and holds its Timeline projection for that History session. This provides exact
temporary-Actor replay today. A server-side elapsed-range slice with lifecycle carry-in
and carry-out is the planned scaling replacement for very large Runs.

Canonical IDs, watermarks, cursors, token integers, and elapsed nanoseconds stay
as decimal strings or `bigint`. Only a viewport-relative, range-checked delta is
converted to a JavaScript number for pixels. Reload reconstructs state from the
server; diagnostic content exists only in the current page memory.

## Compatibility and content safety

Interactive V1 supports Chromium and Edge 111 or newer, Firefox 115 or newer,
and Safari 16.4 or newer, including their corresponding mobile engines. It
requires native `fetch`, `EventSource`, and `BigInt`. Missing browser capability
or a major event, API, stream-control, or UI schema mismatch produces an
explicit static compatibility surface before queries or live ingestion begin.

The server declares `security_scope="trusted_network"`. Pages use same-origin
relative routes and load no external script, font, or data. Captured strings are
rendered as text rather than HTML or executable markup. Content Security Policy,
`X-Content-Type-Options: nosniff`, `Referrer-Policy: no-referrer`, and
same-origin resource policy constrain the embedded interface. This content
safety does not make captured diagnostic data suitable for an untrusted
network.
