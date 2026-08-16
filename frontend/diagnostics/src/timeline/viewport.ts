import {
  type U64String,
  decodeU64,
} from "../protocol/decimal.ts";


export const MAX_TIMELINE_CSS_PIXELS = 1_000_000;
const SUBPIXELS = 1_024n;
const ZOOM_SCALE = 1_000_000n;

export interface TimelineTimeViewport {
  readonly start_ns: U64String;
  readonly end_ns: U64String;
  readonly width_px: number;
  readonly follow_live: boolean;
}

function canonicalU64(value: bigint, label: string): U64String {
  return decodeU64(value.toString(), label);
}

function validateWidth(width: number): void {
  if (!Number.isFinite(width) || width <= 0 || width > MAX_TIMELINE_CSS_PIXELS) {
    throw new RangeError("timeline viewport width is out of bounds");
  }
}

export function createTimelineViewport(
  startNs: U64String,
  endNs: U64String,
  widthPx: number,
  followLive = false,
): TimelineTimeViewport {
  validateWidth(widthPx);
  if (BigInt(startNs) > BigInt(endNs)) {
    throw new RangeError("timeline viewport range is reversed");
  }
  return {
    start_ns: startNs,
    end_ns: endNs,
    width_px: widthPx,
    follow_live: followLive,
  };
}

export function elapsedToPixel(
  elapsedNs: U64String,
  viewport: TimelineTimeViewport,
): number {
  const start = BigInt(viewport.start_ns);
  const end = BigInt(viewport.end_ns);
  const duration = end - start;
  if (duration === 0n) {
    return 0;
  }
  const elapsed = BigInt(elapsedNs);
  const clamped = elapsed < start ? start : elapsed > end ? end : elapsed;
  const widthSubpixels = BigInt(Math.round(viewport.width_px * Number(SUBPIXELS)));
  const result = ((clamped - start) * widthSubpixels) / duration;
  return Number(result) / Number(SUBPIXELS);
}

export function pixelToElapsed(
  pixel: number,
  viewport: TimelineTimeViewport,
): U64String {
  if (!Number.isFinite(pixel)) {
    throw new RangeError("timeline pixel must be finite");
  }
  const start = BigInt(viewport.start_ns);
  const duration = BigInt(viewport.end_ns) - start;
  if (duration === 0n) {
    return viewport.start_ns;
  }
  const clamped = Math.max(0, Math.min(viewport.width_px, pixel));
  const pixelSubpixels = BigInt(Math.round(clamped * Number(SUBPIXELS)));
  const widthSubpixels = BigInt(Math.round(viewport.width_px * Number(SUBPIXELS)));
  return canonicalU64(start + (duration * pixelSubpixels) / widthSubpixels, "timeline.pixel_time");
}

function clampRange(
  start: bigint,
  duration: bigint,
  liveNow: bigint,
): readonly [bigint, bigint] {
  const boundedDuration = duration > liveNow ? liveNow : duration;
  const maximumStart = liveNow - boundedDuration;
  const boundedStart = start < 0n ? 0n : start > maximumStart ? maximumStart : start;
  return [boundedStart, boundedStart + boundedDuration];
}

export function panTimelineViewport(
  viewport: TimelineTimeViewport,
  deltaPixels: number,
  liveNowNs: U64String,
): TimelineTimeViewport {
  if (!Number.isFinite(deltaPixels)) {
    throw new RangeError("timeline pan delta must be finite");
  }
  const start = BigInt(viewport.start_ns);
  const duration = BigInt(viewport.end_ns) - start;
  const widthSubpixels = BigInt(Math.round(viewport.width_px * Number(SUBPIXELS)));
  const deltaSubpixels = BigInt(Math.round(deltaPixels * Number(SUBPIXELS)));
  const elapsedDelta = widthSubpixels === 0n ? 0n : (duration * deltaSubpixels) / widthSubpixels;
  const [nextStart, nextEnd] = clampRange(start + elapsedDelta, duration, BigInt(liveNowNs));
  return createTimelineViewport(
    canonicalU64(nextStart, "timeline.pan.start"),
    canonicalU64(nextEnd, "timeline.pan.end"),
    viewport.width_px,
    false,
  );
}

export function zoomTimelineViewport(
  viewport: TimelineTimeViewport,
  factor: number,
  anchorPixel: number,
  liveNowNs: U64String,
): TimelineTimeViewport {
  if (!Number.isFinite(factor) || factor <= 0 || factor > 100) {
    throw new RangeError("timeline zoom factor is out of bounds");
  }
  const start = BigInt(viewport.start_ns);
  const duration = BigInt(viewport.end_ns) - start;
  const liveNow = BigInt(liveNowNs);
  const scaledFactor = BigInt(Math.max(1, Math.round(factor * Number(ZOOM_SCALE))));
  let nextDuration = (duration * scaledFactor) / ZOOM_SCALE;
  if (nextDuration < 1n) {
    nextDuration = 1n;
  }
  if (nextDuration > liveNow) {
    nextDuration = liveNow;
  }
  const anchor = BigInt(pixelToElapsed(anchorPixel, viewport));
  const anchorOffset = duration === 0n ? 0n : ((anchor - start) * nextDuration) / duration;
  const [nextStart, nextEnd] = clampRange(anchor - anchorOffset, nextDuration, liveNow);
  return createTimelineViewport(
    canonicalU64(nextStart, "timeline.zoom.start"),
    canonicalU64(nextEnd, "timeline.zoom.end"),
    viewport.width_px,
    false,
  );
}

export function followTimelineViewport(
  viewport: TimelineTimeViewport,
  liveNowNs: U64String,
): TimelineTimeViewport {
  const duration = BigInt(viewport.end_ns) - BigInt(viewport.start_ns);
  const liveNow = BigInt(liveNowNs);
  const [start, end] = clampRange(liveNow - duration, duration, liveNow);
  return createTimelineViewport(
    canonicalU64(start, "timeline.follow.start"),
    canonicalU64(end, "timeline.follow.end"),
    viewport.width_px,
    true,
  );
}

export function intervalIntersectsViewport(
  startNs: U64String,
  endNs: U64String,
  viewport: TimelineTimeViewport,
): boolean {
  const start = BigInt(startNs);
  const end = BigInt(endNs);
  return start <= BigInt(viewport.end_ns) && end >= BigInt(viewport.start_ns);
}
