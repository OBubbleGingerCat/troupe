export type TimelineMode = "live" | "history";
export type LifecycleOutcome = "completed" | "failed" | "cancelled";
export type LifecycleState = "future" | "active" | LifecycleOutcome;
export type SceneTone = "green" | "blue" | "amber" | "violet";

export interface SceneRecord {
  readonly id: string;
  readonly label: string;
  readonly start: number;
  readonly end: number | null;
  readonly outcome: LifecycleOutcome | null;
  readonly tone: SceneTone;
}

export interface ActorRecord {
  readonly id: string;
  readonly name: string;
  readonly role: string;
  readonly start: number;
  readonly end: number | null;
  readonly outcome: LifecycleOutcome | null;
  readonly liveSlot: number;
  /** False means this row was recovered without its Actor lifetime span. */
  readonly lifetimeObserved?: boolean;
  /** Most recent scoped evidence for a row recovered without its lifetime span. */
  readonly lastObserved?: number;
}

export type DiagnosticAttribute = string | number | boolean | readonly (string | number | boolean)[];
export type CustomSeverity = "debug" | "info" | "warning" | "error";

export interface SystemEventRecord {
  readonly id: string;
  readonly at: number;
  readonly kind: "tool" | "message";
  readonly label: string;
  readonly actId: string;
  readonly outcome: LifecycleOutcome | null;
}

export interface ActRecord {
  readonly id: string;
  readonly label: string;
  readonly cueId: string;
  readonly start: number;
  readonly end: number | null;
  readonly outcome: LifecycleOutcome | null;
}

export interface CustomSpanRecord {
  readonly id: string;
  readonly name: string;
  readonly cueId: string;
  readonly actId: string | null;
  readonly parentSpanId: string | null;
  readonly start: number;
  readonly end: number | null;
  readonly outcome: LifecycleOutcome | null;
  readonly attributes: Readonly<Record<string, DiagnosticAttribute>>;
}

export interface CustomEventRecord {
  readonly id: string;
  readonly name: string;
  readonly cueId: string;
  readonly actId: string | null;
  readonly containingSpanId: string | null;
  readonly at: number;
  readonly severity: CustomSeverity | null;
  readonly attributes: Readonly<Record<string, DiagnosticAttribute>>;
}

export interface CueRecord {
  readonly id: string;
  readonly label: string;
  readonly sceneId: string;
  readonly actorId: string;
  readonly admitted: number;
  readonly execution: number;
  readonly end: number | null;
  readonly outcome: LifecycleOutcome | null;
  readonly events: readonly SystemEventRecord[];
  /** False means no mailbox-wait or execution lifecycle span was observed for this Cue. */
  readonly lifecycleObserved?: boolean;
  /** Most recent scoped evidence for a Cue recovered without its lifecycle spans. */
  readonly lastObserved?: number;
}

export interface TimelineRange {
  readonly start: number;
  readonly end: number;
}

/** Runtime data consumed by the production Actor-centered timeline. */
export interface TimelineData {
  readonly scenes: readonly SceneRecord[];
  readonly actors: readonly ActorRecord[];
  readonly cues: readonly CueRecord[];
  readonly acts: readonly ActRecord[];
  readonly customSpans: readonly CustomSpanRecord[];
  readonly customEvents: readonly CustomEventRecord[];
  readonly totalTime: number;
  readonly liveNow: number;
  readonly watermark: string;
  readonly productionName: string;
  readonly connectionLabel: string;
  readonly outcomeLabel: string;
  readonly liveSlotCount?: number;
}

export function lifecycleState(
  start: number,
  end: number | null,
  outcome: LifecycleOutcome | null,
  cursor: number,
): LifecycleState {
  if (cursor < start) {
    return "future";
  }
  if (end === null || cursor < end) {
    return "active";
  }
  return outcome ?? "completed";
}

export function actorState(actor: ActorRecord, cursor: number): LifecycleState {
  return lifecycleState(actor.start, actor.end, actor.outcome, cursor);
}

export function cueState(cue: CueRecord, cursor: number): LifecycleState {
  return lifecycleState(cue.admitted, cue.end, cue.outcome, cursor);
}

export function intersects(
  start: number,
  end: number | null,
  range: TimelineRange,
): boolean {
  return start <= range.end && (end ?? Number.POSITIVE_INFINITY) >= range.start;
}

/** Keep NOW at the right edge of a fixed-width rolling Live window. */
export function liveTimelineRange(now: number, windowSeconds: number): TimelineRange {
  const duration = Math.max(1, windowSeconds);
  const cursor = Math.max(0, now);
  return {
    start: cursor - duration,
    end: cursor,
  };
}

/** Clamp a fixed-width History viewport to the recorded Run interval. */
export function historyTimelineRange(
  totalTime: number,
  requestedStart: number,
  windowSeconds: number,
): TimelineRange {
  const runEnd = Math.max(0, totalTime);
  const duration = Math.min(runEnd, Math.max(1, windowSeconds));
  const maximumStart = Math.max(0, runEnd - duration);
  const start = Math.min(maximumStart, Math.max(0, requestedStart));
  return { start, end: start + duration };
}

export function liveActorVisible(
  actor: ActorRecord,
  now: number,
  windowSeconds: number,
  selectedActorId: string | null,
): boolean {
  if (actor.start > now) {
    return false;
  }
  const range = { start: Math.max(0, now - windowSeconds), end: now };
  if (actor.end !== null && actor.end <= now) {
    return false;
  }
  if (actor.lifetimeObserved === false) {
    return actor.id === selectedActorId || (actor.lastObserved ?? actor.start) >= range.start;
  }
  return actor.id === selectedActorId || actor.end === null;
}

export function formatElapsed(seconds: number): string {
  const whole = Math.max(0, Math.round(seconds));
  const minutes = Math.floor(whole / 60);
  const remainder = whole % 60;
  return `${minutes}:${remainder.toString().padStart(2, "0")}`;
}
