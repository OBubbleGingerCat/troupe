import { describe, expect, it } from "vitest";

import {
  decodeCanonicalUuid,
  decodeU64,
} from "../../src/protocol/decimal.ts";
import {
  type DiagnosticEvent,
  type DiagnosticScope,
  decodeDiagnosticEvent,
} from "../../src/protocol/event.ts";
import type { SelectionReference } from "../../src/state/model.ts";
import {
  createDiagnosticState,
  reduceDiagnosticState,
} from "../../src/state/reducer.ts";
import {
  type TimelineModel,
  type TimelineNode,
  layoutTimeline,
  selectTimelineModel,
} from "../../src/timeline/layout.ts";
import {
  type TimelinePrimitive,
  assignTimelineLanes,
} from "../../src/timeline/lanes.ts";
import {
  createTimelineViewport,
  elapsedToPixel,
  followTimelineViewport,
  panTimelineViewport,
  pixelToElapsed,
  zoomTimelineViewport,
} from "../../src/timeline/viewport.ts";


const RUN_ID = decodeCanonicalUuid("12345678-1234-4234-9234-123456789abc");

function selection(id: string): SelectionReference {
  return { kind: "scope", id };
}

function node(
  id: string,
  parentId: string | null,
  kind: TimelineNode["kind"],
  expanded = true,
): TimelineNode {
  return {
    id,
    parent_id: parentId,
    kind,
    label: id,
    status: kind === "cue" ? "waiting" : null,
    selection: selection(id),
    expanded,
  };
}

function primitive(
  id: string,
  rowId: string,
  track: TimelinePrimitive["track"],
  start: number,
  end: number | null,
  order: number,
  kind: TimelinePrimitive["kind"] = "span",
): TimelinePrimitive {
  return {
    id,
    row_id: rowId,
    track,
    kind,
    label: id,
    start_ns: decodeU64(String(start)),
    end_ns: end === null ? null : decodeU64(String(end)),
    order: decodeU64(String(order)),
    status: end === null ? "running" : "completed",
    selection: selection(id),
  };
}

function model(
  nodes: readonly TimelineNode[],
  primitives: readonly TimelinePrimitive[] = [],
  liveNow = 100,
): TimelineModel {
  return {
    nodes,
    primitives,
    live_now_ns: decodeU64(String(liveNow)),
    needs_server_refresh: false,
  };
}

function laneSignature(primitives: readonly TimelinePrimitive[]) {
  return [...(assignTimelineLanes(primitives, decodeU64("100")).get("act")?.assignments ?? [])]
    .sort((left, right) => left.primitive.id.localeCompare(right.primitive.id))
    .map((assignment) => ({
      id: assignment.primitive.id,
      lane: assignment.lane,
      slot: assignment.slot,
      track: assignment.track_index,
    }));
}

function scope(cueId: string, actId: string): DiagnosticScope {
  return {
    scene_id: "scene-1",
    actor_id: "actor-1",
    cue_id: cueId,
    effect_id: null,
    act_id: actId,
    tool_call_id: null,
    session_generation: decodeU64("1"),
  };
}

function event(
  sequence: number,
  eventScope: DiagnosticScope,
  fields: Readonly<Record<string, unknown>>,
): DiagnosticEvent {
  return decodeDiagnosticEvent({
    schema_version: 1,
    run_id: RUN_ID,
    sequence: String(sequence),
    elapsed_ns: String(sequence * 100),
    scope: eventScope,
    caused_by: [],
    ...fields,
  });
}

describe("timeline layout and viewport", () => {
  it("assigns deterministic lowest available lanes and isolates caller and turn tracks", () => {
    const primitives = [
      primitive("life-a", "act", "lifecycle", 10, 40, 2),
      primitive("life-b", "act", "lifecycle", 20, 30, 1),
      primitive("life-open", "act", "lifecycle", 30, null, 3),
      primitive("life-d", "act", "lifecycle", 40, 50, 4),
      primitive("caller", "act", "caller", 15, 80, 5),
      primitive("turn", "act", "turn", 15, 70, 6),
      primitive("fact", "act", "fact", 25, 25, 7, "instant"),
    ];

    const signature = laneSignature(primitives);
    expect(signature).toEqual(laneSignature([...primitives].reverse()));
    expect(signature).toContainEqual({ id: "life-a", lane: 0, slot: 0, track: 0 });
    expect(signature).toContainEqual({ id: "life-b", lane: 1, slot: 1, track: 0 });
    expect(signature).toContainEqual({ id: "life-open", lane: 1, slot: 1, track: 0 });
    expect(signature).toContainEqual({ id: "life-d", lane: 0, slot: 0, track: 0 });
    expect(signature).toContainEqual({ id: "caller", lane: 0, slot: 2, track: 1 });
    expect(signature).toContainEqual({ id: "turn", lane: 0, slot: 3, track: 2 });
    expect(signature).toContainEqual({ id: "fact", lane: 0, slot: 4, track: 3 });
  });

  it("keeps same-Actor Cue branches independent and permits collapsed descendants", () => {
    const nodes = [
      node("production", null, "production"),
      node("actor", "production", "actor"),
      node("cue-a", "actor", "cue", false),
      node("act-a", "cue-a", "act"),
      node("cue-b", "actor", "cue"),
      node("act-b", "cue-b", "act"),
    ];

    const collapsed = layoutTimeline(model(nodes), { scroll_top: 0, height: 192 });
    expect(collapsed.rows.map((row) => row.node.id)).toEqual([
      "production",
      "actor",
      "cue-a",
      "cue-b",
      "act-b",
    ]);
    expect(collapsed.rows.find((row) => row.node.id === "cue-a")?.has_children).toBe(true);

    const expanded = layoutTimeline(model(nodes.map((item) => (
      item.id === "cue-a" ? { ...item, expanded: true } : item
    ))), { scroll_top: 0, height: 192 });
    expect(expanded.rows.map((row) => row.node.id)).toEqual([
      "production",
      "actor",
      "cue-a",
      "act-a",
      "cue-b",
      "act-b",
    ]);
  });

  it("uses fixed-height vertical virtualization with bounded overscan", () => {
    const nodes = [
      node("production", null, "production"),
      ...Array.from({ length: 30 }, (_, index) => (
        node(`fact-${index}`, "production", "fact", false)
      )),
    ];
    const layout = layoutTimeline(model(nodes), { scroll_top: 320, height: 64 });

    expect(layout.row_height).toBe(32);
    expect(layout.total_height).toBe(31 * 32);
    expect(layout.visible_rows.map((row) => row.index)).toEqual([8, 9, 10, 11, 12, 13]);
  });

  it("keeps u64 time exact while pan, zoom, and follow convert only bounded pixels", () => {
    const start = decodeU64("18446744073709500000");
    const end = decodeU64("18446744073709510000");
    const liveNow = decodeU64("18446744073709551615");
    const viewport = createTimelineViewport(start, end, 1_000);

    expect(elapsedToPixel(decodeU64("18446744073709505000"), viewport)).toBe(500);
    expect(pixelToElapsed(500, viewport)).toBe("18446744073709505000");
    expect(panTimelineViewport(viewport, 100, liveNow)).toMatchObject({
      start_ns: "18446744073709501000",
      end_ns: "18446744073709511000",
      follow_live: false,
    });
    expect(zoomTimelineViewport(viewport, 0.5, 500, liveNow)).toMatchObject({
      start_ns: "18446744073709502500",
      end_ns: "18446744073709507500",
    });
    expect(followTimelineViewport(viewport, liveNow)).toMatchObject({
      start_ns: "18446744073709541615",
      end_ns: "18446744073709551615",
      follow_live: true,
    });
  });

  it("selects bounded projections into independent Cue rows and non-nested facts", () => {
    const cueA = scope("cue-a", "act-a");
    const cueB = scope("cue-b", "act-b");
    const events = [
      event(1, cueA, {
        kind: "span_started",
        span_kind: "act.lifecycle",
        detail: { provider: "codex", effective_model: "gpt-5", effective_effort: "high" },
        parent_span_id: null,
      }),
      event(2, cueB, {
        kind: "span_started",
        span_kind: "act.lifecycle",
        detail: { provider: "codex", effective_model: "gpt-5", effective_effort: "high" },
        parent_span_id: null,
      }),
      event(3, cueA, {
        kind: "context_usage_sampled",
        context_used_tokens: "300",
        context_window_tokens: "1000",
        cumulative_cost_amount: null,
        cumulative_cost_currency: null,
        sample_origin: "provider",
        observed_elapsed_ns: null,
      }),
      event(4, cueA, {
        kind: "act_token_usage_finalized",
        availability: "available",
        source: "acp.prompt_response.usage",
        unavailable_reason: null,
        provider_total_tokens: "500",
        input_tokens: "300",
        output_tokens: "200",
        thought_tokens: null,
        cached_read_tokens: null,
        cached_write_tokens: null,
      }),
    ];
    const state = events.reduce(
      (current, item) => reduceDiagnosticState(current, { type: "event_received", event: item }),
      createDiagnosticState(RUN_ID, decodeU64("0")),
    );
    const timeline = selectTimelineModel(state, "example-production");
    const cueNodes = timeline.nodes.filter((item) => item.kind === "cue");

    expect(cueNodes.map((item) => item.id)).toEqual([
      JSON.stringify(["cue", "scene-1", "actor-1", "cue-a"]),
      JSON.stringify(["cue", "scene-1", "actor-1", "cue-b"]),
    ]);
    expect(new Set(cueNodes.map((item) => item.selection.id)).size).toBe(2);
    expect(timeline.primitives.find((item) => item.id === "span:1")?.row_id).not.toBe(
      timeline.primitives.find((item) => item.id === "span:2")?.row_id,
    );
    expect(timeline.primitives).toEqual(expect.arrayContaining([
      expect.objectContaining({ id: "context-usage:3", track: "fact", kind: "counter" }),
      expect.objectContaining({ id: "act-usage:4", track: "fact", kind: "counter" }),
    ]));
  });
});
