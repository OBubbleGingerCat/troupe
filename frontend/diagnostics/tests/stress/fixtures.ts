import { h, render } from "preact";

import { decodeCanonicalUuid, decodeU64 } from "../../src/protocol/decimal.ts";
import { decodeDiagnosticEvent, type DiagnosticEvent } from "../../src/protocol/event.ts";
import {
  ACT_USAGE_CAPACITY,
  ADJACENT_WINDOW_CAPACITY,
  CONTEXT_USAGE_CAPACITY,
  GAP_CAPACITY,
  LIVE_EDGE_EVENT_CAPACITY,
  MESSAGE_CAPACITY,
  QUERY_RESULT_CAPACITY,
  RESULT_FACT_CAPACITY,
  SPAN_CAPACITY,
  TOOL_FACT_CAPACITY,
  type DiagnosticState,
} from "../../src/state/model.ts";
import {
  createDiagnosticState,
  presentedLiveEdge,
  reduceDiagnosticState,
} from "../../src/state/reducer.ts";
import { TimelineCanvas, drawTimelineCanvas } from "../../src/timeline/canvas.tsx";
import {
  buildTimelineHitIndex,
  hitTestTimelineLane,
} from "../../src/timeline/hit_test.ts";
import {
  layoutTimeline,
  type TimelineModel,
  type TimelineNode,
} from "../../src/timeline/layout.ts";
import type { TimelinePrimitive } from "../../src/timeline/lanes.ts";
import { createTimelineViewport } from "../../src/timeline/viewport.ts";


export const STRESS_WORKLOAD = Object.freeze({
  long_run_events: 12_000,
  pause_events: 4_000,
  visible_primitives: 10_000,
  hit_tests: 2_000,
  rerenders_per_frame: 100,
  query_entries: 80,
  activated_windows: 8,
});

export interface StressDurations {
  readonly state_reduce_ms: number;
  readonly pause_reduce_ms: number;
  readonly timeline_layout_ms: number;
  readonly timeline_draw_ms: number;
  readonly hit_test_ms: number;
  readonly raf_updates_ms: number;
}

export interface StressInvariants {
  readonly live_events: number;
  readonly span_items: number;
  readonly message_items: number;
  readonly context_usage_items: number;
  readonly act_usage_items: number;
  readonly tool_items: number;
  readonly result_items: number;
  readonly gap_items: number;
  readonly query_entries: number;
  readonly adjacent_windows: number;
  readonly selection_preserved: boolean;
  readonly span_pair_complete: boolean;
  readonly usage_coverage_complete: boolean;
  readonly gap_state_visible: boolean;
  readonly pause_frozen: boolean;
  readonly pause_unseen_count: string;
  readonly resume_request_kind: string | null;
  readonly resume_query_consumed: boolean;
  readonly raw_backlog_events: number;
  readonly visible_primitives: number;
  readonly drawn_primitives: number;
  readonly hit_examined_max: number;
  readonly raf_callbacks_pending: number;
  readonly raf_draws: number;
  readonly canvas_nonblank: boolean;
}

export interface StressBrowserResult {
  readonly durations_ms: StressDurations;
  readonly invariants: StressInvariants;
}

interface RetainedStressState {
  readonly state: DiagnosticState;
  readonly model: TimelineModel;
  readonly layout: ReturnType<typeof layoutTimeline>;
  readonly hitIndex: ReturnType<typeof buildTimelineHitIndex>;
}

const RUN_ID = decodeCanonicalUuid("12345678-1234-4234-9234-123456789abc");
let retained: RetainedStressState | null = null;

function scopeFor(sequence: number) {
  const block = Math.floor((sequence - 1) / 10);
  return {
    scene_id: `scene-${block % 7}`,
    actor_id: `actor-${block % 13}`,
    cue_id: `cue-${block}`,
    effect_id: null,
    act_id: `act-${block}`,
    tool_call_id: null,
    session_generation: "1",
  };
}

function eventAt(sequence: number): DiagnosticEvent {
  const scope = scopeFor(sequence);
  const blockStart = sequence - ((sequence - 1) % 10);
  const common = {
    schema_version: 1,
    run_id: RUN_ID,
    sequence: String(sequence),
    elapsed_ns: String(sequence * 10),
    scope,
    caused_by: [],
  };
  switch (sequence % 10) {
    case 1:
      return decodeDiagnosticEvent({
        ...common,
        kind: "span_started",
        span_kind: "act.lifecycle",
        detail: {
          provider: "stress",
          effective_model: "diagnostic-model",
          effective_effort: "medium",
        },
        parent_span_id: null,
      });
    case 2:
      return decodeDiagnosticEvent({
        ...common,
        kind: "span_finished",
        span_id: String(blockStart),
        outcome: "completed",
        error_code: null,
      });
    case 3:
      return decodeDiagnosticEvent({
        ...common,
        kind: "agent_message_delta",
        message_id: `message-${blockStart}`,
        source_message_id: null,
        text_delta: "x",
      });
    case 4:
      return decodeDiagnosticEvent({
        ...common,
        kind: "agent_message_completed",
        message_id: `message-${blockStart}`,
        utf8_bytes: "1",
        unicode_scalar_count: "1",
        truncated: false,
      });
    case 5:
      return decodeDiagnosticEvent({
        ...common,
        kind: "custom_counter_sampled",
        name: "stress.queue_depth",
        value: { type: "integer", value: String(sequence) },
        unit: "items",
        dimensions: {},
      });
    case 6:
      return decodeDiagnosticEvent({
        ...common,
        kind: "context_usage_sampled",
        context_used_tokens: String(sequence),
        context_window_tokens: "100000",
        cumulative_cost_amount: null,
        cumulative_cost_currency: null,
        sample_origin: "provider",
        observed_elapsed_ns: null,
      });
    case 7:
      return decodeDiagnosticEvent({
        ...common,
        kind: "act_token_usage_finalized",
        availability: "partial",
        source: "acp.prompt_response.usage",
        unavailable_reason: null,
        provider_total_tokens: null,
        input_tokens: String(sequence),
        output_tokens: null,
        thought_tokens: null,
        cached_read_tokens: null,
        cached_write_tokens: null,
      });
    case 8:
      return decodeDiagnosticEvent({
        ...common,
        kind: "observation_gap",
        producer: "runtime",
        component: null,
        reason: "stress-gap",
        dropped_count: "1",
        affected_elapsed: null,
        affected_kind: null,
        affected_scope: null,
      });
    case 9:
      return decodeDiagnosticEvent({
        ...common,
        scope: { ...scope, tool_call_id: `tool-${blockStart}` },
        kind: "instant_occurred",
        instant_kind: "tool.updated",
        detail: {
          title: `Tool ${blockStart}`,
          tool_kind: "read",
          status: "in_progress",
          error_code: null,
        },
        containing_span_id: null,
      });
    default:
      return decodeDiagnosticEvent({
        ...common,
        kind: "instant_occurred",
        instant_kind: "result.accepted",
        detail: { issue: null, error_code: null },
        containing_span_id: null,
      });
  }
}

function reduceEvents(
  initial: DiagnosticState,
  first: number,
  last: number,
): DiagnosticState {
  let state = initial;
  for (let sequence = first; sequence <= last; sequence += 1) {
    state = reduceDiagnosticState(state, {
      type: "event_received",
      event: eventAt(sequence),
    });
  }
  return state;
}

function addQueries(state: DiagnosticState): DiagnosticState {
  let next = state;
  for (let index = 0; index < STRESS_WORKLOAD.query_entries; index += 1) {
    next = reduceDiagnosticState(next, {
      type: "query_cached",
      result: {
        key: `query-${index}`,
        captured_through: state.cursor.committed_watermark,
        value: { index },
        stale: false,
        invalidated_through: null,
        dependency: { event_kinds: null, scope: null, elapsed_range: null },
      },
    });
  }
  return next;
}

function activateWindows(state: DiagnosticState): DiagnosticState {
  let next = state;
  for (let index = 0; index < STRESS_WORKLOAD.activated_windows; index += 1) {
    next = reduceDiagnosticState(next, {
      type: "window_activated",
      window: {
        id: `window-${index}`,
        run_id: RUN_ID,
        start_ns: decodeU64("0"),
        end_ns: next.live.observed_elapsed_ns,
        captured_through: next.cursor.committed_watermark,
        events: [],
      },
    });
  }
  return next;
}

function timelineFixture(): {
  readonly model: TimelineModel;
  readonly primitives: readonly TimelinePrimitive[];
} {
  const root: TimelineNode = {
    id: "stress-root",
    parent_id: null,
    kind: "production",
    label: "Stress production",
    status: "running",
    selection: { kind: "scope", id: "stress-root" },
    expanded: true,
  };
  const primitives = Array.from(
    { length: STRESS_WORKLOAD.visible_primitives },
    (_, index): TimelinePrimitive => ({
      id: `stress-span-${index}`,
      row_id: root.id,
      track: "lifecycle",
      kind: "span",
      label: `Stress span ${index}`,
      start_ns: decodeU64(String(index * 2)),
      end_ns: decodeU64(String(index * 2 + 1)),
      order: decodeU64(String(index + 1)),
      status: "completed",
      selection: { kind: "span", id: String(index + 1) },
    }),
  );
  return {
    model: {
      nodes: [root],
      primitives,
      live_now_ns: decodeU64(String(STRESS_WORKLOAD.visible_primitives * 2 + 1)),
      needs_server_refresh: false,
    },
    primitives,
  };
}

function runTimelineStress(): {
  readonly model: TimelineModel;
  readonly layout: ReturnType<typeof layoutTimeline>;
  readonly hitIndex: ReturnType<typeof buildTimelineHitIndex>;
  readonly durations: Pick<
    StressDurations,
    "timeline_layout_ms" | "timeline_draw_ms" | "hit_test_ms" | "raf_updates_ms"
  >;
  readonly invariants: Pick<
    StressInvariants,
    | "visible_primitives"
    | "drawn_primitives"
    | "hit_examined_max"
    | "raf_callbacks_pending"
    | "raf_draws"
    | "canvas_nonblank"
  >;
} {
  const fixture = timelineFixture();
  const layoutStarted = performance.now();
  const layout = layoutTimeline(fixture.model, { scroll_top: 0, height: 32 });
  const hitIndex = buildTimelineHitIndex(layout);
  const timelineLayoutMs = performance.now() - layoutStarted;
  const viewport = createTimelineViewport(
    decodeU64("0"),
    decodeU64(String(STRESS_WORKLOAD.visible_primitives * 2)),
    1_200,
  );

  const directCanvas = document.createElement("canvas");
  directCanvas.width = 1_200;
  directCanvas.height = 32;
  const directContext = directCanvas.getContext("2d");
  if (directContext === null) {
    throw new Error("Chromium did not provide a 2D canvas context");
  }
  const drawStarted = performance.now();
  const drawReport = drawTimelineCanvas(
    directContext,
    layout,
    viewport,
    null,
    null,
    1_200,
    32,
    1,
  );
  const timelineDrawMs = performance.now() - drawStarted;

  let examinedMax = 0;
  const hitStarted = performance.now();
  for (let index = 0; index < STRESS_WORKLOAD.hit_tests; index += 1) {
    const primitive = (index * 7919) % STRESS_WORKLOAD.visible_primitives;
    const hit = hitTestTimelineLane(
      hitIndex,
      "stress-root",
      0,
      decodeU64(String(primitive * 2)),
    );
    if (hit === null) {
      throw new Error(`indexed hit ${primitive} was not found`);
    }
    examinedMax = Math.max(examinedMax, hit.examined);
  }
  const hitTestMs = performance.now() - hitStarted;

  const callbacks = new Map<number, FrameRequestCallback>();
  let frameId = 0;
  const nativeRequest = globalThis.requestAnimationFrame;
  const nativeCancel = globalThis.cancelAnimationFrame;
  globalThis.requestAnimationFrame = (callback: FrameRequestCallback): number => {
    frameId += 1;
    callbacks.set(frameId, callback);
    return frameId;
  };
  globalThis.cancelAnimationFrame = (id: number): void => {
    callbacks.delete(id);
  };
  const mount = document.querySelector("#stress-canvas");
  if (!(mount instanceof HTMLElement)) {
    throw new Error("stress canvas mount is unavailable");
  }
  const baseProps = {
    layout,
    viewport,
    hit_index: hitIndex,
    width: 1_200,
    height: 32,
    device_pixel_ratio: 1,
  } as const;
  const rafStarted = performance.now();
  render(h(TimelineCanvas, { ...baseProps, selection: null }), mount);
  for (let index = 0; index < STRESS_WORKLOAD.rerenders_per_frame; index += 1) {
    render(h(TimelineCanvas, {
      ...baseProps,
      selection: { kind: "span", id: String(index + 1) },
    }), mount);
  }
  const pending = callbacks.size;
  const renderedCanvas = mount.querySelector("canvas");
  if (!(renderedCanvas instanceof HTMLCanvasElement)) {
    throw new Error("TimelineCanvas did not mount a canvas");
  }
  const renderedContext = renderedCanvas.getContext("2d");
  if (renderedContext === null) {
    throw new Error("TimelineCanvas did not provide a 2D context");
  }
  const nativeClear = renderedContext.clearRect.bind(renderedContext);
  let rafDraws = 0;
  renderedContext.clearRect = (...arguments_: Parameters<CanvasRenderingContext2D["clearRect"]>) => {
    rafDraws += 1;
    nativeClear(...arguments_);
  };
  for (const callback of callbacks.values()) {
    callback(performance.now());
  }
  callbacks.clear();
  renderedContext.clearRect = nativeClear;
  globalThis.requestAnimationFrame = nativeRequest;
  globalThis.cancelAnimationFrame = nativeCancel;
  const rafUpdatesMs = performance.now() - rafStarted;
  const pixels = directContext.getImageData(0, 0, directCanvas.width, directCanvas.height).data;
  let painted = 0;
  for (let offset = 0; offset < pixels.length; offset += 4) {
    if (pixels[offset] !== 255 || pixels[offset + 1] !== 255 || pixels[offset + 2] !== 255) {
      painted += 1;
    }
  }

  return {
    model: fixture.model,
    layout,
    hitIndex,
    durations: {
      timeline_layout_ms: timelineLayoutMs,
      timeline_draw_ms: timelineDrawMs,
      hit_test_ms: hitTestMs,
      raf_updates_ms: rafUpdatesMs,
    },
    invariants: {
      visible_primitives: layout.lanes_by_row.get("stress-root")?.assignments.length ?? 0,
      drawn_primitives: drawReport.drawn_primitives,
      hit_examined_max: examinedMax,
      raf_callbacks_pending: pending,
      raf_draws: rafDraws,
      canvas_nonblank: painted > 0 && renderedCanvas.width > 0 && renderedCanvas.height > 0,
    },
  };
}

function runStress(): StressBrowserResult {
  let state = createDiagnosticState(RUN_ID, decodeU64("0"));
  state = reduceDiagnosticState(state, {
    type: "select",
    selection: { kind: "span", id: "selected-stress-span" },
  });
  const stateStarted = performance.now();
  state = reduceEvents(state, 1, STRESS_WORKLOAD.long_run_events);
  const stateReduceMs = performance.now() - stateStarted;
  state = addQueries(state);
  state = activateWindows(state);
  state = reduceDiagnosticState(state, { type: "pause" });
  const frozen = presentedLiveEdge(state);
  const pauseStarted = performance.now();
  state = reduceEvents(
    state,
    STRESS_WORKLOAD.long_run_events + 1,
    STRESS_WORKLOAD.long_run_events + STRESS_WORKLOAD.pause_events,
  );
  const pauseReduceMs = performance.now() - pauseStarted;
  const pauseFrozen = presentedLiveEdge(state) === frozen;
  const pauseUnseenCount = state.pause.unseen_count.toString();
  state = reduceDiagnosticState(state, { type: "resume" });
  const resumeRequestKind = state.pause.resume_request?.kind ?? null;
  state = reduceDiagnosticState(state, {
    type: "window_activated",
    window: {
      id: "resume-query-window",
      run_id: RUN_ID,
      start_ns: decodeU64("0"),
      end_ns: state.live.observed_elapsed_ns,
      captured_through: state.cursor.committed_watermark,
      events: [],
    },
  });
  state = reduceDiagnosticState(state, { type: "resume_request_consumed" });

  const totalEvents = STRESS_WORKLOAD.long_run_events + STRESS_WORKLOAD.pause_events;
  const lastBlockStart = totalEvents - 9;
  const lastSpan = state.live.projection.spans.items.find(
    (candidate) => candidate.span_id === String(lastBlockStart),
  );
  const lastUsageSequence = totalEvents - 3;
  const usage = state.live.projection.act_usage.items.find(
    (candidate) => candidate.event.sequence === String(lastUsageSequence),
  );
  const timeline = runTimelineStress();
  retained = { state, model: timeline.model, layout: timeline.layout, hitIndex: timeline.hitIndex };
  return {
    durations_ms: {
      state_reduce_ms: stateReduceMs,
      pause_reduce_ms: pauseReduceMs,
      ...timeline.durations,
    },
    invariants: {
      live_events: state.live.events.length,
      span_items: state.live.projection.spans.items.length,
      message_items: state.live.projection.messages.items.length,
      context_usage_items: state.live.projection.context_usage.items.length,
      act_usage_items: state.live.projection.act_usage.items.length,
      tool_items: state.live.projection.tools.items.length,
      result_items: state.live.projection.results.items.length,
      gap_items: state.live.projection.gaps.items.length,
      query_entries: state.queries.entries.size,
      adjacent_windows: state.windows.adjacent.entries.size,
      selection_preserved: state.presentation.selection?.kind === "span"
        && state.presentation.selection.id === "selected-stress-span",
      span_pair_complete: lastSpan?.start?.sequence === String(lastBlockStart)
        && lastSpan.finish?.sequence === String(lastBlockStart + 1),
      usage_coverage_complete: usage?.event.input_tokens === String(lastUsageSequence)
        && state.live.projection.context_usage.items.length > 0,
      gap_state_visible: state.live.projection.gaps.declared_dropped_count > 0n
        && state.live.projection.gaps.needs_server_refresh,
      pause_frozen: pauseFrozen,
      pause_unseen_count: pauseUnseenCount,
      resume_request_kind: resumeRequestKind,
      resume_query_consumed: state.pause.resume_request === null,
      raw_backlog_events: state.live.events.length,
      ...timeline.invariants,
    },
  };
}

function releaseStress(): void {
  retained = null;
  const mount = document.querySelector("#stress-canvas");
  if (mount instanceof HTMLElement) {
    render(null, mount);
    mount.replaceChildren();
  }
}

export function expectedStressInvariants(): Record<string, number | string | boolean> {
  return {
    live_events: LIVE_EDGE_EVENT_CAPACITY,
    span_items_max: SPAN_CAPACITY,
    message_items_max: MESSAGE_CAPACITY,
    context_usage_items_max: CONTEXT_USAGE_CAPACITY,
    act_usage_items_max: ACT_USAGE_CAPACITY,
    tool_items_max: TOOL_FACT_CAPACITY,
    result_items_max: RESULT_FACT_CAPACITY,
    gap_items_max: GAP_CAPACITY,
    query_entries: QUERY_RESULT_CAPACITY,
    adjacent_windows: ADJACENT_WINDOW_CAPACITY,
    pause_unseen_count: String(STRESS_WORKLOAD.pause_events),
    visible_primitives: STRESS_WORKLOAD.visible_primitives,
    drawn_primitives: STRESS_WORKLOAD.visible_primitives,
    hit_examined_max: 1,
    raf_callbacks_pending_max: 2,
    raf_draws: 1,
  };
}

export function installStressFixture(): void {
  const host = document.querySelector("#app");
  if (!(host instanceof HTMLElement)) {
    throw new Error("stress fixture root is unavailable");
  }
  const main = document.createElement("main");
  const heading = document.createElement("h1");
  heading.textContent = "Diagnostics stress fixture";
  const canvasHost = document.createElement("div");
  canvasHost.id = "stress-canvas";
  main.append(heading, canvasHost);
  host.replaceChildren(main);
  Object.defineProperty(globalThis, "__v05", {
    configurable: true,
    value: { run: runStress, release: releaseStress },
  });
}

declare global {
  var __v05: {
    readonly run: () => StressBrowserResult;
    readonly release: () => void;
  };
}
