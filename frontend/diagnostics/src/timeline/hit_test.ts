import type { U64String } from "../protocol/decimal.ts";
import type { SelectionReference } from "../state/model.ts";
import type { TimelineLayout } from "./layout.ts";
import type { TimelineLaneAssignment } from "./lanes.ts";
import {
  type TimelineTimeViewport,
  pixelToElapsed,
} from "./viewport.ts";


export const TIMELINE_LANE_PADDING = 4;

interface IndexedInterval {
  readonly assignment: TimelineLaneAssignment;
  readonly start: bigint;
  readonly end: bigint;
  readonly instant: boolean;
}

interface LaneIndex {
  readonly starts: readonly bigint[];
  readonly intervals: readonly IndexedInterval[];
}

export interface TimelineHitIndex {
  readonly rows: ReadonlyMap<string, ReadonlyMap<number, LaneIndex>>;
}

export interface TimelineHit {
  readonly primitive_id: string;
  readonly selection: SelectionReference;
  readonly row_id: string;
  readonly slot: number;
  readonly examined: number;
}

function intervalFor(
  assignment: TimelineLaneAssignment,
  liveNow: bigint,
): IndexedInterval {
  const start = BigInt(assignment.primitive.start_ns);
  const explicitEnd = assignment.primitive.end_ns;
  const end = explicitEnd === null ? (liveNow > start ? liveNow : start) : BigInt(explicitEnd);
  if (end < start) {
    throw new RangeError(`timeline hit interval is reversed: ${assignment.primitive.id}`);
  }
  return {
    assignment,
    start,
    end,
    instant: assignment.primitive.kind !== "span" || end === start,
  };
}

export function buildTimelineHitIndex(layout: TimelineLayout): TimelineHitIndex {
  const rows = new Map<string, ReadonlyMap<number, LaneIndex>>();
  const liveNow = BigInt(layout.model.live_now_ns);
  for (const [rowId, rowLanes] of layout.lanes_by_row) {
    const bySlot = new Map<number, IndexedInterval[]>();
    for (const assignment of rowLanes.assignments) {
      const intervals = bySlot.get(assignment.slot);
      const interval = intervalFor(assignment, liveNow);
      if (intervals === undefined) {
        bySlot.set(assignment.slot, [interval]);
      } else {
        intervals.push(interval);
      }
    }
    const laneIndexes = new Map<number, LaneIndex>();
    for (const [slot, intervals] of bySlot) {
      intervals.sort((left, right) => {
        if (left.start !== right.start) {
          return left.start < right.start ? -1 : 1;
        }
        const leftOrder = BigInt(left.assignment.primitive.order);
        const rightOrder = BigInt(right.assignment.primitive.order);
        if (leftOrder !== rightOrder) {
          return leftOrder < rightOrder ? -1 : 1;
        }
        return left.assignment.primitive.id.localeCompare(right.assignment.primitive.id);
      });
      laneIndexes.set(slot, {
        starts: intervals.map((interval) => interval.start),
        intervals,
      });
    }
    rows.set(rowId, laneIndexes);
  }
  return { rows };
}

function upperBound(values: readonly bigint[], target: bigint): number {
  let low = 0;
  let high = values.length;
  while (low < high) {
    const middle = low + Math.floor((high - low) / 2);
    if (values[middle]! <= target) {
      low = middle + 1;
    } else {
      high = middle;
    }
  }
  return low;
}

export function hitTestTimelineLane(
  index: TimelineHitIndex,
  rowId: string,
  slot: number,
  elapsedNs: U64String,
): TimelineHit | null {
  const lane = index.rows.get(rowId)?.get(slot);
  if (lane === undefined || lane.intervals.length === 0) {
    return null;
  }
  const elapsed = BigInt(elapsedNs);
  const candidateIndex = upperBound(lane.starts, elapsed) - 1;
  if (candidateIndex < 0) {
    return null;
  }
  const candidate = lane.intervals[candidateIndex]!;
  const contains = candidate.instant
    ? candidate.start === elapsed
    : candidate.start <= elapsed && elapsed <= candidate.end;
  if (!contains) {
    return null;
  }
  return {
    primitive_id: candidate.assignment.primitive.id,
    selection: candidate.assignment.primitive.selection,
    row_id: rowId,
    slot,
    examined: 1,
  };
}

export function hitTestTimelinePoint(
  layout: TimelineLayout,
  index: TimelineHitIndex,
  viewport: TimelineTimeViewport,
  x: number,
  y: number,
): TimelineHit | null {
  if (
    !Number.isFinite(x)
    || !Number.isFinite(y)
    || x < 0
    || x > viewport.width_px
    || y < 0
    || y >= layout.viewport_height
  ) {
    return null;
  }
  const absoluteY = layout.scroll_top + y;
  const rowIndex = Math.floor(absoluteY / layout.row_height);
  const row = layout.rows[rowIndex];
  if (row === undefined) {
    return null;
  }
  const lanes = layout.lanes_by_row.get(row.node.id);
  if (lanes === undefined) {
    return null;
  }
  const drawableHeight = row.height - TIMELINE_LANE_PADDING * 2;
  const withinRow = absoluteY - row.top - TIMELINE_LANE_PADDING;
  if (withinRow < 0 || withinRow > drawableHeight) {
    return null;
  }
  const slot = Math.min(
    lanes.total_slots - 1,
    Math.floor((withinRow / drawableHeight) * lanes.total_slots),
  );
  return hitTestTimelineLane(index, row.node.id, slot, pixelToElapsed(x, viewport));
}
