import { decodeCanonicalUuid, decodeU64 } from "../../src/protocol/decimal.ts";
import {
  decodeDiagnosticEvent,
  type DiagnosticEvent,
  type DiagnosticScope,
} from "../../src/protocol/event.ts";
import {
  createDiagnosticState,
  reduceDiagnosticState,
} from "../../src/state/reducer.ts";
import { liveActorVisible } from "../../src/timeline/actor_timeline_model.ts";
import {
  selectCapturedTimelineData,
  selectProductionTimelineData,
} from "../../src/timeline/production_timeline.ts";


const RUN_ID = decodeCanonicalUuid("12345678-1234-4234-9234-123456789abc");

function scope(
  sceneId: string | null,
  actorId: string | null = null,
  cueId: string | null = null,
  actId: string | null = null,
): DiagnosticScope {
  return {
    scene_id: sceneId,
    actor_id: actorId,
    cue_id: cueId,
    effect_id: null,
    act_id: actId,
    tool_call_id: null,
    session_generation: actorId === null ? null : decodeU64("1"),
  };
}

function event(
  sequence: number,
  elapsedSeconds: number,
  eventScope: DiagnosticScope,
  fields: Readonly<Record<string, unknown>>,
): DiagnosticEvent {
  return decodeDiagnosticEvent({
    schema_version: 1,
    run_id: RUN_ID,
    sequence: String(sequence),
    elapsed_ns: String(elapsedSeconds * 1_000_000_000),
    scope: eventScope,
    caused_by: [],
    ...fields,
  });
}

function ingest(events: readonly DiagnosticEvent[]) {
  return events.reduce(
    (state, item) => reduceDiagnosticState(state, { type: "event_received", event: item }),
    createDiagnosticState(RUN_ID, decodeU64("0")),
  );
}

describe("production Actor timeline projection", () => {
  it("places built-in lifecycles and Python diagnostics on one elapsed-time plane", () => {
    const scene = scope("scene-1");
    const actor = scope("scene-1", "actor-1");
    const cue = scope("scene-1", "actor-1", "cue-1");
    const act = scope("scene-1", "actor-1", "cue-1", "act-1");
    const state = ingest([
      event(1, 1, scene, {
        kind: "span_started",
        span_kind: "scene.lifecycle",
        detail: {},
        parent_span_id: null,
      }),
      event(2, 2, actor, {
        kind: "span_started",
        span_kind: "actor.handle_lifetime",
        detail: { display_name: "Researcher", actor_type: "ResearchActor" },
        parent_span_id: "1",
      }),
      event(3, 3, cue, {
        kind: "instant_occurred",
        instant_kind: "cue.admitted",
        detail: {},
        containing_span_id: "2",
      }),
      event(4, 4, cue, {
        kind: "span_started",
        span_kind: "cue.mailbox_wait",
        detail: {},
        parent_span_id: "2",
      }),
      event(5, 5, cue, {
        kind: "span_finished",
        span_id: "4",
        outcome: "completed",
        error_code: null,
      }),
      event(6, 6, cue, {
        kind: "span_started",
        span_kind: "cue.execution",
        detail: {},
        parent_span_id: "2",
      }),
      event(7, 7, act, {
        kind: "span_started",
        span_kind: "act.lifecycle",
        detail: { provider: "fixture", effective_model: "model-a", effective_effort: "high" },
        parent_span_id: "6",
      }),
      event(8, 8, act, {
        kind: "custom_span_started",
        name: "example.fetch_context",
        parent_span_id: "7",
        attributes: {
          batch: { type: "integer", value: "4" },
          region: { type: "string", value: "eu-west" },
        },
      }),
      event(9, 9, act, {
        kind: "custom_instant_occurred",
        name: "example.context_ready",
        containing_span_id: "8",
        severity: "info",
        attributes: { records: { type: "integer", value: "12" } },
      }),
      event(10, 10, act, {
        kind: "custom_span_finished",
        span_id: "8",
        outcome: "completed",
      }),
      event(11, 11, act, {
        kind: "span_finished",
        span_id: "7",
        outcome: "completed",
        error_code: null,
      }),
      event(12, 12, cue, {
        kind: "span_finished",
        span_id: "6",
        outcome: "completed",
        error_code: null,
      }),
      event(13, 13, actor, {
        kind: "span_finished",
        span_id: "2",
        outcome: "completed",
        error_code: null,
      }),
      event(14, 14, scene, {
        kind: "span_finished",
        span_id: "1",
        outcome: "completed",
        error_code: null,
      }),
    ]);

    const data = selectProductionTimelineData(
      state,
      { connection: "connected", outcome: "completed" },
      { productionName: "Fixture production" },
    );

    expect(data.productionName).toBe("Fixture production");
    expect(data.liveNow).toBe(14);
    expect(data.scenes).toEqual([expect.objectContaining({
      id: "scene-1",
      start: 1,
      end: 14,
      outcome: "completed",
    })]);
    expect(data.actors).toEqual([expect.objectContaining({
      id: "actor-1",
      name: "Researcher",
      role: "ResearchActor",
      start: 2,
      end: 13,
      outcome: "completed",
    })]);
    expect(data.cues).toEqual([expect.objectContaining({
      id: "cue-1",
      admitted: 3,
      execution: 6,
      end: 12,
      outcome: "completed",
    })]);
    expect(data.acts).toEqual([expect.objectContaining({
      id: "act-1",
      label: "Act act-1 · model-a",
      start: 7,
      end: 11,
    })]);
    expect(data.customSpans).toEqual([expect.objectContaining({
      id: "8",
      name: "example.fetch_context",
      cueId: "cue-1",
      actId: "act-1",
      start: 8,
      end: 10,
      attributes: { batch: 4, region: "eu-west" },
    })]);
    expect(data.customEvents).toEqual([expect.objectContaining({
      id: "9",
      name: "example.context_ready",
      containingSpanId: "8",
      at: 9,
      severity: "info",
      attributes: { records: 12 },
    })]);
    expect(liveActorVisible(data.actors[0]!, 80, 60, null)).toBe(false);
  });

  it("keeps an actor row when a bounded projection lacks its lifetime span", () => {
    const cue = scope("scene-2", "actor-fallback", "cue-2");
    const state = ingest([
      event(1, 20, cue, {
        kind: "span_started",
        span_kind: "cue.execution",
        detail: {},
        parent_span_id: null,
      }),
    ]);

    const data = selectProductionTimelineData(
      state,
      { connection: "archive", outcome: "incomplete" },
    );

    expect(data.actors).toEqual([expect.objectContaining({
      id: "actor-fallback",
      name: "actor-fallback",
      start: 20,
      end: null,
    })]);
  });

  it("retains completed temporary Actors in a frozen History capture beyond Live span capacity", () => {
    const captured: DiagnosticEvent[] = [];
    let sequence = 1;
    for (let index = 0; index < 300; index += 1) {
      const actor = scope(`scene-${index}`, `temporary-${index}`);
      const start = sequence;
      captured.push(event(sequence, index * 2 + 1, actor, {
        kind: "span_started",
        span_kind: "actor.handle_lifetime",
        detail: { display_name: `Temporary ${index}`, actor_type: "EphemeralActor" },
        parent_span_id: null,
      }));
      sequence += 1;
      captured.push(event(sequence, index * 2 + 1.5, actor, {
        kind: "span_finished",
        span_id: String(start),
        outcome: "completed",
        error_code: null,
      }));
      sequence += 1;
    }

    const data = selectCapturedTimelineData(captured, decodeU64(String(sequence - 1)), {
      productionName: "History fixture",
      connectionLabel: "Archive",
      outcomeLabel: "completed",
    });

    expect(data.actors).toHaveLength(300);
    expect(data.actors[0]).toEqual(expect.objectContaining({
      id: "temporary-0",
      name: "Temporary 0",
      end: 1.5,
    }));
    expect(data.actors[data.actors.length - 1]).toEqual(expect.objectContaining({
      id: "temporary-299",
      name: "Temporary 299",
      end: 599.5,
    }));
  });
});
