import { decodeCanonicalUuid, decodeU64 } from "../../src/protocol/decimal.ts";
import {
  decodeDiagnosticEvent,
  type DiagnosticEvent,
  type DiagnosticScope,
} from "../../src/protocol/event.ts";
import {
  VISIBLE_WINDOW_EVENT_CAPACITY,
} from "../../src/state/model.ts";
import {
  createDiagnosticState,
  reduceDiagnosticState,
} from "../../src/state/reducer.ts";
import { activateWindow, createEventWindow } from "../../src/state/windows.ts";
import {
  historyTimelineRange,
  liveActorVisible,
  liveTimelineRange,
} from "../../src/timeline/actor_timeline_model.ts";
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
    elapsed_ns: String(Math.round(elapsedSeconds * 1_000_000_000)),
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
  it("clips the merged bootstrap and hot edges to one visible event capacity", () => {
    const capacity = VISIBLE_WINDOW_EVENT_CAPACITY;
    const oldEvents = Array.from({ length: capacity }, (_, index) => {
      const sequence = index + 1;
      return event(sequence, sequence, scope(`scene-${sequence}`), {
        kind: "custom_instant_occurred",
        name: "example.old_window",
        containing_span_id: null,
        severity: "debug",
        attributes: {},
      });
    });
    let state = createDiagnosticState(
      RUN_ID,
      decodeU64(String(capacity)),
      oldEvents[oldEvents.length - 1]!.elapsed_ns,
    );
    for (let sequence = capacity + 1; sequence <= capacity * 2; sequence += 1) {
      state = reduceDiagnosticState(state, {
        type: "event_received",
        event: event(sequence, sequence, scope(`scene-${sequence}`), {
          kind: "custom_instant_occurred",
          name: "example.hot_edge",
          containing_span_id: null,
          severity: "debug",
          attributes: {},
        }),
      });
    }
    state = {
      ...state,
      windows: activateWindow(state.windows, createEventWindow({
        id: "bootstrap-window",
        run_id: RUN_ID,
        start_ns: oldEvents[0]!.elapsed_ns,
        end_ns: oldEvents[oldEvents.length - 1]!.elapsed_ns,
        captured_through: decodeU64(String(capacity * 2)),
        events: oldEvents,
      })),
    };

    const data = selectProductionTimelineData(
      state,
      { connection: "connected", outcome: "running" },
    );
    expect(data.scenes).toHaveLength(capacity);
    expect(data.scenes[0]?.id).toBe(`scene-${capacity + 1}`);
    expect(data.scenes[data.scenes.length - 1]?.id).toBe(`scene-${capacity * 2}`);
    expect(data.scenes.some((scene) => scene.id === `scene-${capacity}`)).toBe(false);
  });

  it("deduplicates overlapping bootstrap and hot event windows", () => {
    const events = [1, 2, 3].map((sequence) => event(
      sequence,
      sequence,
      scope(`scene-${sequence}`),
      {
        kind: "custom_instant_occurred",
        name: "example.window_overlap",
        containing_span_id: null,
        severity: "debug",
        attributes: {},
      },
    ));
    let state = ingest(events);
    state = {
      ...state,
      windows: activateWindow(state.windows, createEventWindow({
        id: "overlapping-bootstrap-window",
        run_id: RUN_ID,
        start_ns: events[0]!.elapsed_ns,
        end_ns: events[1]!.elapsed_ns,
        captured_through: events[1]!.sequence,
        events: events.slice(0, 2),
      })),
    };

    const data = selectProductionTimelineData(
      state,
      { connection: "connected", outcome: "running" },
    );
    expect(data.scenes.map((scene) => scene.id)).toEqual([
      "scene-1",
      "scene-2",
      "scene-3",
    ]);
  });

  it("keeps a fixed-width Live scale before and after the window starts rolling", () => {
    expect(liveTimelineRange(0, 60)).toEqual({ start: -60, end: 0 });
    expect(liveTimelineRange(10, 60)).toEqual({ start: -50, end: 10 });
    expect(liveTimelineRange(59.9, 60).start).toBeCloseTo(-0.1);
    expect(liveTimelineRange(59.9, 60).end).toBe(59.9);
    expect(liveTimelineRange(60, 60)).toEqual({ start: 0, end: 60 });
    expect(liveTimelineRange(75, 60)).toEqual({ start: 15, end: 75 });
  });

  it("keeps a fixed-width History viewport while clamping it to the Run", () => {
    expect(historyTimelineRange(100, 45, 10)).toEqual({ start: 45, end: 55 });
    expect(historyTimelineRange(100, -5, 10)).toEqual({ start: 0, end: 10 });
    expect(historyTimelineRange(100, 95, 10)).toEqual({ start: 90, end: 100 });
    expect(historyTimelineRange(6, 2, 10)).toEqual({ start: 0, end: 6 });
    expect(historyTimelineRange(0, 10, 10)).toEqual({ start: 0, end: 0 });
  });

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

  it("marks event-only Cues when a long Live window outlasts the span projection", () => {
    const captured: DiagnosticEvent[] = [];
    let sequence = 1;
    const actor = scope("scene-long", "actor-long");
    const pushCueAdmission = (cueId: string, at: number): void => {
      captured.push(event(sequence, at, { ...actor, cue_id: cueId }, {
        kind: "instant_occurred",
        instant_kind: "cue.admitted",
        detail: {},
        containing_span_id: null,
      }));
      sequence += 1;
    };

    const pushCompletedCue = (cueId: string, admitted: number): void => {
      const cueScope = { ...actor, cue_id: cueId };
      pushCueAdmission(cueId, admitted);
      const waitStart = String(sequence);
      captured.push(event(sequence, admitted + 0.01, cueScope, {
        kind: "span_started",
        span_kind: "cue.mailbox_wait",
        detail: {},
        parent_span_id: null,
      }));
      sequence += 1;
      captured.push(event(sequence, admitted + 0.02, cueScope, {
        kind: "span_finished",
        span_id: waitStart,
        outcome: "completed",
        error_code: null,
      }));
      sequence += 1;
      const executionStart = String(sequence);
      captured.push(event(sequence, admitted + 0.03, cueScope, {
        kind: "span_started",
        span_kind: "cue.execution",
        detail: {},
        parent_span_id: null,
      }));
      sequence += 1;
      captured.push(event(sequence, admitted + 0.04, cueScope, {
        kind: "span_finished",
        span_id: executionStart,
        outcome: "completed",
        error_code: null,
      }));
      sequence += 1;
    };

    pushCompletedCue("cue-1", 0.05);
    pushCompletedCue("cue-2", 0.1);
    for (let index = 3; index <= 60; index += 1) {
      pushCueAdmission(`cue-${index}`, index);
    }

    let fillerSpans = 0;
    while (sequence <= 3336) {
      if (fillerSpans < 300 && sequence + 1 <= 3336) {
        const fillerStart = String(sequence);
        captured.push(event(sequence, sequence * 0.7, scope("scene-filler"), {
          kind: "custom_span_started",
          name: "example.filler_span",
          parent_span_id: null,
          attributes: {},
        }));
        sequence += 1;
        captured.push(event(sequence, sequence * 0.7, scope("scene-filler"), {
          kind: "custom_span_finished",
          span_id: fillerStart,
          outcome: "completed",
        }));
        sequence += 1;
        fillerSpans += 1;
      } else {
        captured.push(event(sequence, sequence * 0.7, scope("scene-filler"), {
          kind: "custom_instant_occurred",
          name: "example.filler_event",
          containing_span_id: null,
          severity: "debug",
          attributes: {},
        }));
        sequence += 1;
      }
    }
    pushCueAdmission("cue-61", 2432.8);
    pushCueAdmission("cue-62", 2432.866);
    expect(sequence).toBe(3339);

    const base = ingest(captured);
    expect(base.live.projection.spans.items.some((span) => (
      span.start?.scope.cue_id === "cue-1"
    ))).toBe(false);
    const state = {
      ...base,
      windows: activateWindow(base.windows, createEventWindow({
        id: "long-run-prefix",
        run_id: RUN_ID,
        start_ns: decodeU64("0"),
        end_ns: captured[captured.length - 1]!.elapsed_ns,
        captured_through: decodeU64(String(captured.length)),
        events: captured,
      })),
    };
    const data = selectProductionTimelineData(
      state,
      { connection: "connected", outcome: "running" },
    );

    expect(data.cues).toHaveLength(62);
    expect(data.cues.find((cue) => cue.id === "cue-1")).toEqual(expect.objectContaining({
      admitted: 0.05,
      lifecycleObserved: true,
      lastObserved: 0.09,
      execution: 0.08,
      end: 0.09,
    }));
    expect(data.cues.find((cue) => cue.id === "cue-62")).toEqual(expect.objectContaining({
      admitted: 2432.866,
      lifecycleObserved: false,
      lastObserved: 2432.866,
    }));
  });
});
