# Troupe examples

These production packages progress from one Actor to cooperative scheduling and
structured cancellation. Run the commands from the repository root after
installing Troupe in the active Python environment.

These examples cast Codex-backed Actors, so Node.js, npm, and `npx` must be
installed and Codex must already be logged in through its CLI. The agent session
starts at cast time even when an example does not call `Actor.act()`.

Four examples print their result once and then keep that Scene active until
`Ctrl+C`. The repeating-Scenes and diagnostics examples instead return from each
Scene so Troupe starts the next one. On `Ctrl+C`, Troupe cancels the current
Scene, drains its Actor work, and calls `stop()` before the command exits.

`ActorHandle.cue()` is typed as returning `tuple[Effect, ...]`. Examples that
own both the Actor implementation and its caller use `typing.cast()` to tell the
static checker which concrete Effect that implementation returns. The cast does
not perform a runtime check.

## 1. Hello Actor

Cast one Actor, send one Cue, and receive a user-defined Effect:

```console
troupe --production examples/hello_actor -- Ada
```

Expected output:

```text
Hello, Ada!
```

## 2. Repeating Scenes

Return from `scene()` and let the Production lifecycle continuously start a new
Scene. There is no loop inside the method; each output line comes from a distinct
Scene call:

```console
troupe --production examples/repeating_scenes
```

The command prints `scene:1`, `scene:2`, and so on until you press `Ctrl+C`.

## 3. Actor pipeline

Route a Cue from one Actor to another, query the Actor registry, and inspect Cue
source and Effect ownership:

```console
troupe --production examples/actor_pipeline -- hello troupe
```

The JSON output shows both Actors, the formatted message, the downstream Cue
source, and the Actor that owns the Effect.

## 4. Cooperative workers

Run work on two Actors concurrently while two requests wait behind the first
request in one Actor's FIFO mailbox:

```console
troupe --production examples/cooperative_workers
```

The timeline shows that the right Actor completes while the left Actor is
waiting, and that the submitted `left:2`, `left:3` requests start in that order
only after `left:1` finishes.

## 5. Cancellation cleanup

Keep an Actor request running, then press `Ctrl+C` to observe structured cleanup:

```console
troupe --production examples/cancellation_cleanup
```

The output order demonstrates that Actor cleanup completes before Scene cleanup
returns and `Production.stop()` runs.

## 6. Live diagnostics showcase

Run a real, continuously repeating Production that is designed to be observed
from the embedded diagnostic Web interface. Each Scene queues two Cues on one
persistent Actor: a read-only shell probe followed by a no-tool recall turn. This
produces distinct mailbox, Cue, Act, message, tool, context, usage, custom-event,
and Effect records while proving that Actor context survives between Cues.

This example starts two real Codex turns per Scene until you press `Ctrl+C`, so it
continuously consumes provider tokens. The final argument is the delay in seconds
after both Acts finish and before `scene()` returns; the default is 30 seconds.

```console
troupe --production examples/diagnostics --diagnostic-bind-host 127.0.0.1 --diagnostic-port 43120 -- 30
```

Open `http://127.0.0.1:43120`. Timeline shows each Scene and both serialized Cues;
Agent streams messages and tool activity; Usage separates context occupancy from
final per-Act accounting; and Views contains the example's Timeline, Metric,
Table, and TimeSeries declarations. The Production prints one compact sink work
summary per Scene. Remove the loopback bind option only when the trusted-LAN,
unauthenticated server is intentionally reachable from other hosts.

While the Production is running, inspect the same Run from another terminal or
export its currently committed prefix as a Perfetto trace:

```shell
troupe diagnostic status --production examples/diagnostics
troupe diagnostic events --production examples/diagnostics --tail 20 --follow
troupe diagnostic dump --production examples/diagnostics --output diagnostics-live.pftrace
```

After `Ctrl+C`, list retained Runs, serve one archive on loopback, or export an
exact archived Run. Replace `RUN_ID` with a value printed by `runs`:

```shell
troupe diagnostic runs --production examples/diagnostics
troupe diagnostic serve --production examples/diagnostics --run RUN_ID --open
troupe diagnostic dump --production examples/diagnostics --run RUN_ID --output diagnostics-archive.pftrace
```

Open either `.pftrace` file manually in the public Perfetto UI. Archive serving
and trace export read persisted diagnostics without restarting the Production or
launching another provider turn.
