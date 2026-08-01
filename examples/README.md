# Troupe examples

These production packages progress from one Actor to cooperative scheduling and
structured cancellation. Run the commands from the repository root after
installing Troupe in the active Python environment.

Four examples print their result once and then keep that Scene active until
`Ctrl+C`. The repeating-Scenes example instead returns from each Scene so Troupe
starts the next one. On `Ctrl+C`, Troupe cancels the current Scene, drains its
Actor work, and calls `stop()` before the command exits.

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
