import { cleanup, fireEvent, render, screen } from "@testing-library/preact";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import { decodeU64 } from "../../src/protocol/decimal.ts";
import type { SelectionReference } from "../../src/state/model.ts";
import { TimelineTreegrid } from "../../src/timeline/aria.tsx";
import {
  TimelineCanvas,
  drawTimelineCanvas,
} from "../../src/timeline/canvas.tsx";
import { buildTimelineHitIndex } from "../../src/timeline/hit_test.ts";
import {
  type TimelineModel,
  type TimelineNode,
  layoutTimeline,
} from "../../src/timeline/layout.ts";
import type { TimelinePrimitive } from "../../src/timeline/lanes.ts";
import { createTimelineViewport } from "../../src/timeline/viewport.ts";


interface FakeCanvas {
  readonly context: CanvasRenderingContext2D;
  readonly commands: string[];
}

let originalGetContext: PropertyDescriptor | undefined;

beforeEach(() => {
  originalGetContext = Object.getOwnPropertyDescriptor(HTMLCanvasElement.prototype, "getContext");
});

afterEach(() => {
  cleanup();
  vi.unstubAllGlobals();
  if (originalGetContext === undefined) {
    Reflect.deleteProperty(HTMLCanvasElement.prototype, "getContext");
  } else {
    Object.defineProperty(HTMLCanvasElement.prototype, "getContext", originalGetContext);
  }
});

function selection(id: string, kind: SelectionReference["kind"] = "scope"): SelectionReference {
  return { kind, id };
}

function node(
  id: string,
  parentId: string | null,
  kind: TimelineNode["kind"],
  expanded: boolean,
  status: TimelineNode["status"] = null,
): TimelineNode {
  return {
    id,
    parent_id: parentId,
    kind,
    label: id,
    status,
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
): TimelinePrimitive {
  return {
    id,
    row_id: rowId,
    track,
    kind: "span",
    label: id,
    start_ns: decodeU64(String(start)),
    end_ns: end === null ? null : decodeU64(String(end)),
    order: decodeU64(String(order)),
    status: end === null ? "running" : "completed",
    selection: selection(id, "span"),
  };
}

function fixture(needsRefresh = true) {
  const nodes = [
    node("production", null, "production", true, "running"),
    node("scene-main", "production", "scene", true),
    node("actor-a", "scene-main", "actor", true),
    node("cue-1", "actor-a", "cue", true, "running"),
    node("act-1", "cue-1", "act", true, "running"),
    node("caller-1", "act-1", "caller", false, "completed"),
    node("turn-1", "act-1", "turn", false, "running"),
    node("tool-1", "act-1", "tool", false, "failed"),
    node("cue-2", "actor-a", "cue", false, "waiting"),
    node("act-2", "cue-2", "act", true, "queued"),
  ];
  const primitives = [
    primitive("run-open", "production", "lifecycle", 10, null, 1),
    primitive("act-life", "act-1", "lifecycle", 20, 80, 2),
    primitive("caller-span", "caller-1", "caller", 25, 45, 3),
    primitive("turn-span", "turn-1", "turn", 30, 70, 4),
    primitive("tool-span", "tool-1", "fact", 40, 60, 5),
    primitive("outside-time", "production", "fact", 500, 550, 6),
  ];
  const model: TimelineModel = {
    nodes,
    primitives,
    live_now_ns: decodeU64("600"),
    needs_server_refresh: needsRefresh,
  };
  const layout = layoutTimeline(model, { scroll_top: 0, height: 288 });
  return {
    layout,
    hitIndex: buildTimelineHitIndex(layout),
    viewport: createTimelineViewport(decodeU64("0"), decodeU64("100"), 400),
  };
}

function fakeCanvas(): FakeCanvas {
  const commands: string[] = [];
  const command = (name: string) => (..._values: readonly unknown[]): void => {
    commands.push(name);
  };
  const context = {
    fillStyle: "",
    strokeStyle: "",
    lineWidth: 1,
    setTransform: command("setTransform"),
    clearRect: command("clearRect"),
    fillRect: command("fillRect"),
    beginPath: command("beginPath"),
    moveTo: command("moveTo"),
    lineTo: command("lineTo"),
    stroke: command("stroke"),
    arc: command("arc"),
    fill: command("fill"),
    strokeRect: command("strokeRect"),
  } as unknown as CanvasRenderingContext2D;
  Object.defineProperty(HTMLCanvasElement.prototype, "getContext", {
    configurable: true,
    value: vi.fn(() => context),
  });
  return { context, commands };
}

describe("timeline Canvas and ARIA semantic surface", () => {
  it("exposes hierarchy, status, selection, expansion, and keyboard navigation", () => {
    const { layout } = fixture();
    const selected = vi.fn();
    const toggled = vi.fn();
    render(
      <TimelineTreegrid
        layout={layout}
        selection={selection("turn-1")}
        onSelect={selected}
        onToggle={toggled}
      />,
    );

    const treegrid = screen.getByRole("treegrid", { name: "Production timeline" });
    const cueRows = screen.getAllByRole("row", { name: /^cue-[12], cue,/ });
    const callerRow = screen.getByRole("row", { name: "caller-1, caller, completed" });
    const turnRow = screen.getByRole("row", { name: "turn-1, turn, running" });
    const actRow = screen.getByRole("row", { name: "act-1, act, running" });

    expect(treegrid.getAttribute("aria-rowcount")).toBe("9");
    expect(cueRows).toHaveLength(2);
    expect(turnRow.getAttribute("aria-level")).toBe("6");
    expect(turnRow.getAttribute("aria-selected")).toBe("true");
    expect(screen.getByRole("status").textContent).toContain("partial");
    expect(screen.getByText("failed").closest("[role=row]")?.getAttribute("data-node-id")).toBe(
      "tool-1",
    );

    callerRow.focus();
    fireEvent.keyDown(callerRow, { key: "ArrowLeft" });
    expect(document.activeElement).toBe(actRow);

    const cue2 = cueRows.find((row) => row.getAttribute("data-node-id") === "cue-2")!;
    fireEvent.keyDown(cue2, { key: "ArrowRight" });
    expect(toggled).toHaveBeenCalledWith("cue-2");

    fireEvent.keyDown(turnRow, { key: "Enter" });
    expect(selected).toHaveBeenCalledWith(selection("turn-1"));
    fireEvent.keyDown(turnRow, { key: "ArrowDown" });
    expect(document.activeElement?.getAttribute("data-node-id")).toBe("tool-1");
  });

  it("synchronizes virtual rows and coalesces model, resize, and hover into one draw per rAF", () => {
    const { layout, hitIndex, viewport } = fixture(false);
    const { commands } = fakeCanvas();
    const callbacks = new Map<number, FrameRequestCallback>();
    let nextFrame = 1;
    vi.stubGlobal("requestAnimationFrame", (callback: FrameRequestCallback): number => {
      const id = nextFrame;
      nextFrame += 1;
      callbacks.set(id, callback);
      return id;
    });
    vi.stubGlobal("cancelAnimationFrame", (id: number): void => {
      callbacks.delete(id);
    });
    const flushFrame = (): void => {
      const entry = callbacks.entries().next().value as [number, FrameRequestCallback] | undefined;
      expect(entry).toBeDefined();
      callbacks.delete(entry![0]);
      entry![1](0);
    };
    const hover = vi.fn();
    const selected = vi.fn();
    const view = render(
      <div>
        <TimelineCanvas
          layout={layout}
          viewport={viewport}
          hit_index={hitIndex}
          selection={null}
          width={400}
          height={288}
          device_pixel_ratio={2}
          onHover={hover}
          onSelect={selected}
        />
        <TimelineTreegrid
          layout={layout}
          selection={null}
          onSelect={selected}
          onToggle={vi.fn()}
        />
      </div>,
    );
    const canvas = view.container.querySelector("canvas")!;
    const treegrid = screen.getByRole("treegrid", { name: "Production timeline" });

    expect(canvas.dataset.visibleRowIds).toBe(treegrid.dataset.visibleRowIds);
    expect(callbacks.size).toBe(1);
    view.rerender(
      <TimelineCanvas
        layout={layout}
        viewport={viewport}
        hit_index={hitIndex}
        selection={selection("run-open", "span")}
        width={401}
        height={288}
        device_pixel_ratio={2}
        onHover={hover}
        onSelect={selected}
      />,
    );
    expect(callbacks.size).toBe(1);
    fireEvent.pointerMove(view.container.querySelector("canvas")!, { clientX: 40, clientY: 16 });
    expect(callbacks.size).toBe(1);

    flushFrame();
    const updatedCanvas = view.container.querySelector("canvas")!;
    expect(updatedCanvas.width).toBe(802);
    expect(updatedCanvas.height).toBe(576);
    expect(commands).toContain("setTransform");
    expect(commands).toContain("fillRect");
    expect(commands.length).toBeGreaterThan(5);
  });

  it("draws only visible-time primitives and keeps open spans at live now", () => {
    const { layout, viewport } = fixture(false);
    const { context, commands } = fakeCanvas();
    const report = drawTimelineCanvas(
      context,
      layout,
      viewport,
      selection("run-open", "span"),
      null,
      400,
      288,
      2,
    );

    expect(report).toEqual({ visible_rows: 9, drawn_primitives: 5 });
    expect(commands.filter((command) => command === "strokeRect")).toHaveLength(1);
    expect(commands).not.toContain("drawImage");
  });
});
