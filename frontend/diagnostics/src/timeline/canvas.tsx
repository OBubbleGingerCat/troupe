import type { JSX } from "preact";
import {
  useEffect,
  useLayoutEffect,
  useRef,
} from "preact/hooks";

import type { SelectionReference } from "../state/model.ts";
import { sameSelectionReference } from "../state/selection.ts";
import {
  TIMELINE_LANE_PADDING,
  type TimelineHitIndex,
  hitTestTimelinePoint,
} from "./hit_test.ts";
import type { TimelineLayout } from "./layout.ts";
import type { TimelineLaneAssignment } from "./lanes.ts";
import {
  type TimelineTimeViewport,
  elapsedToPixel,
  intervalIntersectsViewport,
} from "./viewport.ts";


export interface TimelineCanvasProps {
  readonly layout: TimelineLayout;
  readonly viewport: TimelineTimeViewport;
  readonly hit_index: TimelineHitIndex;
  readonly selection: SelectionReference | null;
  readonly width: number;
  readonly height: number;
  readonly device_pixel_ratio?: number | undefined;
  readonly onHover?: ((selection: SelectionReference | null) => void) | undefined;
  readonly onSelect?: ((selection: SelectionReference) => void) | undefined;
}

export interface TimelineDrawReport {
  readonly visible_rows: number;
  readonly drawn_primitives: number;
}

const TRACK_COLORS = ["#0b6b61", "#a15c00", "#6d4c91", "#35618f"] as const;

function canvasRatio(value: number | undefined): number {
  const ratio = value ?? window.devicePixelRatio;
  if (!Number.isFinite(ratio) || ratio <= 0 || ratio > 8) {
    throw new RangeError("timeline device pixel ratio is out of bounds");
  }
  return ratio;
}

function assignmentEnd(
  assignment: TimelineLaneAssignment,
  liveNow: TimelineLayout["model"]["live_now_ns"],
): TimelineLayout["model"]["live_now_ns"] {
  return assignment.primitive.end_ns ?? liveNow;
}

export function drawTimelineCanvas(
  context: CanvasRenderingContext2D,
  layout: TimelineLayout,
  viewport: TimelineTimeViewport,
  selection: SelectionReference | null,
  hoveredPrimitiveId: string | null,
  width: number,
  height: number,
  ratio: number,
): TimelineDrawReport {
  context.setTransform(ratio, 0, 0, ratio, 0, 0);
  context.clearRect(0, 0, width, height);
  context.fillStyle = "#ffffff";
  context.fillRect(0, 0, width, height);
  let drawn = 0;
  for (const row of layout.visible_rows) {
    const y = row.top - layout.scroll_top;
    if (y + row.height < 0 || y > height) {
      continue;
    }
    context.fillStyle = row.index % 2 === 0 ? "#f7f9f7" : "#eef2ef";
    context.fillRect(0, y, width, row.height);
    context.strokeStyle = "#d3dad4";
    context.lineWidth = 1;
    context.beginPath();
    context.moveTo(0, y + row.height - 0.5);
    context.lineTo(width, y + row.height - 0.5);
    context.stroke();

    const lanes = layout.lanes_by_row.get(row.node.id);
    if (lanes === undefined) {
      continue;
    }
    const drawableHeight = row.height - TIMELINE_LANE_PADDING * 2;
    const slotHeight = drawableHeight / lanes.total_slots;
    for (const assignment of lanes.assignments) {
      const endNs = assignmentEnd(assignment, layout.model.live_now_ns);
      if (!intervalIntersectsViewport(assignment.primitive.start_ns, endNs, viewport)) {
        continue;
      }
      const startX = elapsedToPixel(assignment.primitive.start_ns, viewport);
      const endX = elapsedToPixel(endNs, viewport);
      const laneY = y + TIMELINE_LANE_PADDING + assignment.slot * slotHeight;
      const primitiveHeight = Math.max(2, slotHeight * 0.62);
      const centerY = laneY + slotHeight / 2;
      context.fillStyle = TRACK_COLORS[assignment.track_index] ?? "#4f5d54";
      if (assignment.primitive.kind === "span") {
        context.fillRect(
          startX,
          centerY - primitiveHeight / 2,
          Math.max(2, endX - startX),
          primitiveHeight,
        );
      } else if (assignment.primitive.kind === "counter") {
        context.fillRect(startX - 2, centerY - 2, 4, 4);
      } else {
        context.beginPath();
        context.arc(startX, centerY, 2.5, 0, Math.PI * 2);
        context.fill();
      }
      if (
        hoveredPrimitiveId === assignment.primitive.id
        || sameSelectionReference(selection, assignment.primitive.selection)
      ) {
        context.strokeStyle = "#151a17";
        context.lineWidth = 1.5;
        context.strokeRect(
          startX - 2,
          centerY - primitiveHeight / 2 - 2,
          Math.max(6, endX - startX + 4),
          primitiveHeight + 4,
        );
      }
      drawn += 1;
    }
  }
  return { visible_rows: layout.visible_rows.length, drawn_primitives: drawn };
}

export function TimelineCanvas(props: TimelineCanvasProps): JSX.Element {
  const canvas = useRef<HTMLCanvasElement | null>(null);
  const frame = useRef<number | null>(null);
  const hoveredPrimitive = useRef<string | null>(null);
  const latest = useRef(props);
  latest.current = props;

  const schedule = (): void => {
    if (frame.current !== null) {
      return;
    }
    frame.current = requestAnimationFrame(() => {
      frame.current = null;
      const element = canvas.current;
      const current = latest.current;
      if (element === null) {
        return;
      }
      const ratio = canvasRatio(current.device_pixel_ratio);
      const backingWidth = Math.max(1, Math.round(current.width * ratio));
      const backingHeight = Math.max(1, Math.round(current.height * ratio));
      if (element.width !== backingWidth) {
        element.width = backingWidth;
      }
      if (element.height !== backingHeight) {
        element.height = backingHeight;
      }
      element.style.width = `${current.width}px`;
      element.style.height = `${current.height}px`;
      const context = element.getContext("2d");
      if (context !== null) {
        drawTimelineCanvas(
          context,
          current.layout,
          current.viewport,
          current.selection,
          hoveredPrimitive.current,
          current.width,
          current.height,
          ratio,
        );
      }
    });
  };

  useLayoutEffect(() => {
    schedule();
  });
  useEffect(() => () => {
    if (frame.current !== null) {
      cancelAnimationFrame(frame.current);
      frame.current = null;
    }
  }, []);

  const point = (event: JSX.TargetedPointerEvent<HTMLCanvasElement>) => {
    const element = event.currentTarget;
    const rect = element.getBoundingClientRect();
    const width = rect.width > 0 ? rect.width : props.width;
    const height = rect.height > 0 ? rect.height : props.height;
    const x = ((event.clientX - rect.left) / width) * props.width;
    const y = ((event.clientY - rect.top) / height) * props.height;
    return hitTestTimelinePoint(props.layout, props.hit_index, props.viewport, x, y);
  };

  const onPointerMove = (event: JSX.TargetedPointerEvent<HTMLCanvasElement>): void => {
    const hit = point(event);
    const next = hit?.primitive_id ?? null;
    if (next === hoveredPrimitive.current) {
      return;
    }
    hoveredPrimitive.current = next;
    props.onHover?.(hit?.selection ?? null);
    schedule();
  };

  return (
    <canvas
      ref={canvas}
      class="timeline-canvas"
      aria-hidden="true"
      data-visible-rows={String(props.layout.visible_rows.length)}
      data-visible-row-ids={props.layout.visible_rows.map((row) => row.node.id).join(",")}
      onPointerMove={onPointerMove}
      onPointerLeave={() => {
        if (hoveredPrimitive.current !== null) {
          hoveredPrimitive.current = null;
          props.onHover?.(null);
          schedule();
        }
      }}
      onPointerDown={(event) => {
        const hit = point(event);
        if (hit !== null) {
          props.onSelect?.(hit.selection);
        }
      }}
    />
  );
}
