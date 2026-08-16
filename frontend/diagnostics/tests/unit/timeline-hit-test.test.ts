import { describe, expect, it } from "vitest";

import { decodeU64 } from "../../src/protocol/decimal.ts";
import type { SelectionReference } from "../../src/state/model.ts";
import {
  buildTimelineHitIndex,
  hitTestTimelineLane,
  hitTestTimelinePoint,
} from "../../src/timeline/hit_test.ts";
import {
  type TimelineModel,
  type TimelineNode,
  layoutTimeline,
} from "../../src/timeline/layout.ts";
import type { TimelinePrimitive } from "../../src/timeline/lanes.ts";
import { createTimelineViewport } from "../../src/timeline/viewport.ts";


function selection(id: string): SelectionReference {
  return { kind: "span", id };
}

function node(id: string, parentId: string | null): TimelineNode {
  return {
    id,
    parent_id: parentId,
    kind: parentId === null ? "production" : "act",
    label: id,
    status: null,
    selection: { kind: "scope", id },
    expanded: true,
  };
}

function span(id: string, rowId: string, start: number, end: number | null): TimelinePrimitive {
  return {
    id,
    row_id: rowId,
    track: "lifecycle",
    kind: "span",
    label: id,
    start_ns: decodeU64(String(start)),
    end_ns: end === null ? null : decodeU64(String(end)),
    order: decodeU64(String(start + 1)),
    status: end === null ? "running" : "completed",
    selection: selection(id),
  };
}

function fixture() {
  const primitives = [
    ...Array.from({ length: 100 }, (_, index) => (
      span(`root-${index}`, "root", index * 10, index * 10 + 5)
    )),
    span("other-at-500", "other", 500, 505),
    span("open", "other", 1_000, null),
  ];
  const model: TimelineModel = {
    nodes: [node("root", null), node("other", "root")],
    primitives,
    live_now_ns: decodeU64("2000"),
    needs_server_refresh: false,
  };
  const layout = layoutTimeline(model, { scroll_top: 0, height: 64 });
  return { layout, index: buildTimelineHitIndex(layout) };
}

describe("timeline hit testing", () => {
  it("uses a row-local, lane-local interval index with one examined candidate", () => {
    const { index } = fixture();

    expect(hitTestTimelineLane(index, "root", 0, decodeU64("502"))).toMatchObject({
      primitive_id: "root-50",
      row_id: "root",
      slot: 0,
      examined: 1,
    });
    expect(hitTestTimelineLane(index, "root", 0, decodeU64("508"))).toBeNull();
    expect(hitTestTimelineLane(index, "other", 0, decodeU64("502"))).toMatchObject({
      primitive_id: "other-at-500",
      row_id: "other",
      examined: 1,
    });
    expect(index.rows.get("root")?.get(0)?.intervals).toHaveLength(100);
    expect(index.rows.get("other")?.get(0)?.intervals).toHaveLength(2);
  });

  it("extends open spans only through the presented live edge", () => {
    const { index } = fixture();

    expect(hitTestTimelineLane(index, "other", 0, decodeU64("1500"))?.primitive_id).toBe("open");
    expect(hitTestTimelineLane(index, "other", 0, decodeU64("2000"))?.primitive_id).toBe("open");
    expect(hitTestTimelineLane(index, "other", 0, decodeU64("2001"))).toBeNull();
  });

  it("maps bounded canvas coordinates directly to the matching row and lane", () => {
    const { layout, index } = fixture();
    const viewport = createTimelineViewport(decodeU64("0"), decodeU64("2000"), 200);

    expect(hitTestTimelinePoint(layout, index, viewport, 50.2, 16)).toMatchObject({
      primitive_id: "root-50",
      row_id: "root",
      examined: 1,
    });
    expect(hitTestTimelinePoint(layout, index, viewport, 50.2, 48)).toMatchObject({
      primitive_id: "other-at-500",
      row_id: "other",
      examined: 1,
    });
    expect(hitTestTimelinePoint(layout, index, viewport, -1, 16)).toBeNull();
    expect(hitTestTimelinePoint(layout, index, viewport, 201, 16)).toBeNull();
    expect(hitTestTimelinePoint(layout, index, viewport, 50.2, 64)).toBeNull();
  });
});
