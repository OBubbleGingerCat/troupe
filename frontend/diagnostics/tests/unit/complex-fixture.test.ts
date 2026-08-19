import { decodeU64 } from "../../src/protocol/decimal.ts";
import { createDiagnosticState, reduceDiagnosticState } from "../../src/state/reducer.ts";
import {
  selectCapturedTimelineData,
  selectProductionTimelineData,
} from "../../src/timeline/production_timeline.ts";
import {
  COMPLEX_EVENTS,
  COMPLEX_RUN_ID,
  COMPLEX_WATERMARK,
} from "../fixtures/complex_events.ts";


describe("complex diagnostics timeline fixture", () => {
  it("contains persistent and temporary Actor lifetimes with deep custom telemetry", () => {
    const data = selectCapturedTimelineData(COMPLEX_EVENTS, decodeU64(COMPLEX_WATERMARK), {
      productionName: "Complex timeline fixture",
      connectionLabel: "Archive",
      outcomeLabel: "completed",
    });

    expect(COMPLEX_EVENTS[0]?.run_id).toBe(COMPLEX_RUN_ID);
    expect(COMPLEX_EVENTS.length).toBeGreaterThan(4_096);
    expect(data.scenes).toHaveLength(48);
    expect(data.actors).toHaveLength(51);
    expect(data.actors.filter((actor) => actor.id.startsWith("actor-dynamic-") && actor.end !== null))
      .toHaveLength(48);
    expect(data.actors.filter((actor) => actor.id.startsWith("actor-") && actor.end === null))
      .toHaveLength(3);
    expect(data.cues).toHaveLength(240);
    expect(data.acts).toHaveLength(240);
    expect(data.customSpans.length).toBeGreaterThanOrEqual(720);
    expect(data.customEvents.length).toBeGreaterThanOrEqual(720);
    expect(data.customSpans.some((span) => span.parentSpanId !== null)).toBe(true);
    expect(data.customEvents.some((event) => event.containingSpanId !== null)).toBe(true);
    expect(data.totalTime).toBeGreaterThan(700);
  });

  it("retains open Actor lifetimes and their creation-order slots after span pressure", () => {
    let state = createDiagnosticState(COMPLEX_EVENTS[0]!.run_id, decodeU64("0"));
    for (const event of COMPLEX_EVENTS) {
      state = reduceDiagnosticState(state, { type: "event_received", event });
    }
    const openActorIds = state.live.projection.spans.items.flatMap((span) => (
      span.start?.kind === "span_started"
      && span.start.span_kind === "actor.handle_lifetime"
      && span.finish === null
        ? [span.start.scope.actor_id]
        : []
    ));
    expect(openActorIds).toEqual(["actor-ingest", "actor-review", "actor-publish"]);

    const live = selectProductionTimelineData(
      state,
      { connection: "connected", outcome: "running" },
      { productionName: "Complex timeline fixture" },
    );
    expect(live.actors.filter((actor) => (
      actor.lifetimeObserved === true && actor.end === null
    )).map((actor) => actor.id)).toEqual([
      "actor-ingest",
      "actor-review",
      "actor-publish",
    ]);
    expect(live.scenes.find((scene) => scene.id === "scene-12")).toMatchObject({
      start: 166.4,
      end: 177,
    });
    expect(live.scenes.filter((scene) => scene.end === null)).toEqual([]);
    expect(live.cues.find((cue) => cue.id === "cue-12-ingest-primary")).toMatchObject({
      admitted: 171,
      execution: 171,
      end: 171,
      lifecycleObserved: false,
    });
  });
});
