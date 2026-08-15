import { describe, expect, it } from "vitest";

import { decodeCanonicalUuid, decodeU64 } from "../../src/protocol/decimal.ts";
import { type DiagnosticEvent, decodeDiagnosticEvent } from "../../src/protocol/event.ts";
import {
  ADJACENT_WINDOW_CAPACITY,
  LIVE_EDGE_EVENT_CAPACITY,
} from "../../src/state/model.ts";
import { createDiagnosticState, reduceDiagnosticState } from "../../src/state/reducer.ts";
import {
  activateWindow,
  createEventWindow,
  createWindowState,
  promoteAdjacentWindow,
} from "../../src/state/windows.ts";


const RUN_ID = decodeCanonicalUuid("12345678-1234-4234-9234-123456789abc");

function counter(sequence: number, elapsed = sequence): DiagnosticEvent {
  return decodeDiagnosticEvent({
    schema_version: 1,
    run_id: RUN_ID,
    sequence: String(sequence),
    elapsed_ns: String(elapsed),
    scope: {
      scene_id: "scene-1",
      actor_id: "actor-1",
      cue_id: null,
      effect_id: null,
      act_id: null,
      tool_call_id: null,
      session_generation: null,
    },
    caused_by: [],
    kind: "counter_sampled",
    counter_kind: "actor.mailbox_depth",
    value: String(sequence),
  });
}

function window(index: number) {
  const sequence = index + 1;
  return createEventWindow({
    id: `window-${index}`,
    run_id: RUN_ID,
    start_ns: decodeU64(String(index * 100)),
    end_ns: decodeU64(String(index * 100 + 99)),
    captured_through: decodeU64(String(sequence)),
    events: [counter(sequence, index * 100 + 1)],
  });
}

describe("bounded diagnostic windows", () => {
  it("keeps one visible window and an exact fixed adjacent-window LRU", () => {
    let state = createWindowState();
    for (let index = 0; index < ADJACENT_WINDOW_CAPACITY + 2; index += 1) {
      state = activateWindow(state, window(index));
    }

    expect(state.visible?.id).toBe(`window-${ADJACENT_WINDOW_CAPACITY + 1}`);
    expect(state.adjacent.entries.size).toBe(ADJACENT_WINDOW_CAPACITY);
    expect(state.adjacent.entries.has("window-0")).toBe(false);
    expect(state.adjacent.entries.has("window-1")).toBe(true);

    const promoted = promoteAdjacentWindow(state, "window-2");
    expect(promoted.visible?.id).toBe("window-2");
    expect(promoted.adjacent.entries.has(`window-${ADJACENT_WINDOW_CAPACITY + 1}`)).toBe(true);
  });

  it("rejects non-exact or cross-run window material", () => {
    expect(() => createEventWindow({
      id: "reverse",
      run_id: RUN_ID,
      start_ns: decodeU64("20"),
      end_ns: decodeU64("10"),
      captured_through: decodeU64("1"),
      events: [counter(1)],
    })).toThrowError(/range/);

    const otherRun = decodeCanonicalUuid("aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa");
    expect(() => createEventWindow({
      id: "cross-run",
      run_id: otherRun,
      start_ns: decodeU64("0"),
      end_ns: decodeU64("10"),
      captured_through: decodeU64("1"),
      events: [counter(1)],
    })).toThrowError(/run/);
  });

  it("bounds paused live data and emits an exact server range when unseen data was evicted", () => {
    let state = createDiagnosticState(RUN_ID, decodeU64("0"));
    state = reduceDiagnosticState(state, { type: "pause" });
    for (let sequence = 1; sequence <= LIVE_EDGE_EVENT_CAPACITY + 5; sequence += 1) {
      state = reduceDiagnosticState(state, {
        type: "event_received",
        event: counter(sequence),
      });
    }

    expect(state.pause.paused).toBe(true);
    expect(state.pause.unseen_count).toBe(BigInt(LIVE_EDGE_EVENT_CAPACITY + 5));
    expect(state.live.events).toHaveLength(LIVE_EDGE_EVENT_CAPACITY);
    expect(state.live.dropped_through).toBe("5");

    const resumed = reduceDiagnosticState(state, { type: "resume" });
    expect(resumed.pause).toMatchObject({
      paused: false,
      unseen_count: 0n,
      resume_request: {
        kind: "server_range",
        after_sequence: "0",
        through_sequence: String(LIVE_EDGE_EVENT_CAPACITY + 5),
      },
    });
  });

  it("resumes from the hot edge locally, but queries when committed delivery is missing", () => {
    let hot = createDiagnosticState(RUN_ID, decodeU64("0"));
    hot = reduceDiagnosticState(hot, { type: "pause" });
    for (let sequence = 1; sequence <= 5; sequence += 1) {
      hot = reduceDiagnosticState(hot, { type: "event_received", event: counter(sequence) });
    }
    expect(reduceDiagnosticState(hot, { type: "resume" }).pause.resume_request).toBeNull();

    let missing = createDiagnosticState(RUN_ID, decodeU64("5"));
    missing = reduceDiagnosticState(missing, { type: "pause" });
    missing = reduceDiagnosticState(missing, {
      type: "watermark_observed",
      through_sequence: decodeU64("9"),
    });
    expect(missing.pause.unseen_count).toBe(4n);
    expect(reduceDiagnosticState(missing, { type: "resume" }).pause.resume_request).toEqual({
      kind: "server_range",
      after_sequence: "5",
      through_sequence: "9",
    });
  });

  it("retains identity-only selection after its window leaves the LRU", () => {
    let state = createDiagnosticState(RUN_ID, decodeU64("0"));
    state = reduceDiagnosticState(state, {
      type: "select",
      selection: { kind: "event", id: "sequence:1" },
    });
    state = reduceDiagnosticState(state, {
      type: "pin_detail",
      selection: { kind: "span", id: "span:1" },
    });
    for (let index = 0; index < ADJACENT_WINDOW_CAPACITY + 3; index += 1) {
      state = reduceDiagnosticState(state, {
        type: "window_activated",
        window: window(index),
      });
    }

    expect(state.windows.adjacent.entries.has("window-0")).toBe(false);
    expect(state.presentation.selection).toEqual({ kind: "event", id: "sequence:1" });
    expect(state.presentation.pinned_detail).toEqual({ kind: "span", id: "span:1" });
  });
});
