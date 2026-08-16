import type { U64String } from "../protocol/decimal.ts";
import type { SelectionReference } from "../state/model.ts";


export const TIMELINE_TRACK_ORDER = ["lifecycle", "caller", "turn", "fact"] as const;
export type TimelineTrackKind = typeof TIMELINE_TRACK_ORDER[number];
export type TimelinePrimitiveKind = "span" | "instant" | "counter";

export interface TimelinePrimitive {
  readonly id: string;
  readonly row_id: string;
  readonly track: TimelineTrackKind;
  readonly kind: TimelinePrimitiveKind;
  readonly label: string;
  readonly start_ns: U64String;
  readonly end_ns: U64String | null;
  readonly order: U64String;
  readonly status: string | null;
  readonly selection: SelectionReference;
}

export interface TimelineLaneAssignment {
  readonly primitive: TimelinePrimitive;
  readonly lane: number;
  readonly slot: number;
  readonly track_index: number;
}

export interface TimelineRowLanes {
  readonly row_id: string;
  readonly total_slots: number;
  readonly assignments: readonly TimelineLaneAssignment[];
}

function comparePrimitive(left: TimelinePrimitive, right: TimelinePrimitive): number {
  const leftStart = BigInt(left.start_ns);
  const rightStart = BigInt(right.start_ns);
  if (leftStart !== rightStart) {
    return leftStart < rightStart ? -1 : 1;
  }
  const leftOrder = BigInt(left.order);
  const rightOrder = BigInt(right.order);
  if (leftOrder !== rightOrder) {
    return leftOrder < rightOrder ? -1 : 1;
  }
  return left.id.localeCompare(right.id);
}

function effectiveEnd(primitive: TimelinePrimitive, liveNow: bigint): bigint {
  const start = BigInt(primitive.start_ns);
  if (primitive.end_ns === null) {
    return liveNow > start ? liveNow : start;
  }
  const end = BigInt(primitive.end_ns);
  if (end < start) {
    throw new RangeError(`timeline primitive ends before it starts: ${primitive.id}`);
  }
  return end;
}

function assignTrack(
  primitives: readonly TimelinePrimitive[],
  liveNow: bigint,
): readonly { readonly primitive: TimelinePrimitive; readonly lane: number }[] {
  const laneEnds: bigint[] = [];
  return [...primitives].sort(comparePrimitive).map((primitive) => {
    const start = BigInt(primitive.start_ns);
    const end = effectiveEnd(primitive, liveNow);
    let lane = laneEnds.findIndex((candidate) => candidate <= start);
    if (lane < 0) {
      lane = laneEnds.length;
      laneEnds.push(end);
    } else {
      laneEnds[lane] = end;
    }
    return { primitive, lane };
  });
}

export function assignTimelineLanes(
  primitives: readonly TimelinePrimitive[],
  liveNowNs: U64String,
): ReadonlyMap<string, TimelineRowLanes> {
  const liveNow = BigInt(liveNowNs);
  const byRow = new Map<string, Map<TimelineTrackKind, TimelinePrimitive[]>>();
  const ids = new Set<string>();
  for (const primitive of primitives) {
    if (ids.has(primitive.id)) {
      throw new RangeError(`duplicate timeline primitive identity: ${primitive.id}`);
    }
    ids.add(primitive.id);
    let tracks = byRow.get(primitive.row_id);
    if (tracks === undefined) {
      tracks = new Map();
      byRow.set(primitive.row_id, tracks);
    }
    const track = tracks.get(primitive.track);
    if (track === undefined) {
      tracks.set(primitive.track, [primitive]);
    } else {
      track.push(primitive);
    }
  }

  const rows = new Map<string, TimelineRowLanes>();
  for (const [rowId, tracks] of byRow) {
    const pending: {
      readonly primitive: TimelinePrimitive;
      readonly lane: number;
      readonly track_index: number;
      readonly track: TimelineTrackKind;
    }[] = [];
    const laneCounts = new Map<TimelineTrackKind, number>();
    for (const [trackIndex, track] of TIMELINE_TRACK_ORDER.entries()) {
      const assigned = assignTrack(tracks.get(track) ?? [], liveNow);
      for (const item of assigned) {
        pending.push({ ...item, track_index: trackIndex, track });
        laneCounts.set(track, Math.max(laneCounts.get(track) ?? 0, item.lane + 1));
      }
    }
    let offset = 0;
    const offsets = new Map<TimelineTrackKind, number>();
    for (const track of TIMELINE_TRACK_ORDER) {
      offsets.set(track, offset);
      offset += laneCounts.get(track) ?? 0;
    }
    const assignments = pending.map((item): TimelineLaneAssignment => ({
      primitive: item.primitive,
      lane: item.lane,
      slot: (offsets.get(item.track) ?? 0) + item.lane,
      track_index: item.track_index,
    }));
    rows.set(rowId, {
      row_id: rowId,
      total_slots: Math.max(1, offset),
      assignments,
    });
  }
  return rows;
}
