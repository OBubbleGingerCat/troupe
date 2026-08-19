import { decodeU64 } from "../../src/protocol/decimal.ts";
import { selectCapturedTimelineData } from "../../src/timeline/production_timeline.ts";
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
});
