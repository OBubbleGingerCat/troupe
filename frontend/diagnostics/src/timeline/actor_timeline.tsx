import {
  Activity,
  Archive,
  Braces,
  Clock3,
  Code2,
  History as HistoryIcon,
  MessageSquare,
  Pause,
  Pin,
  Play,
  Radio,
  RotateCcw,
  UserRoundCheck,
  UserRoundPlus,
  UserRoundX,
  Wrench,
  X,
} from "lucide-preact";
import type { JSX } from "preact";
import {
  useEffect,
  useLayoutEffect,
  useMemo,
  useRef,
  useState,
} from "preact/hooks";

import "./actor_timeline.css";
import {
  type TimelineData,
  type ActorRecord,
  type ActRecord,
  type CustomSpanRecord,
  type DiagnosticAttribute,
  type CueRecord,
  type LifecycleState,
  type SceneRecord,
  type SystemEventRecord,
  type TimelineMode,
  type TimelineRange,
  actorState,
  cueState,
  formatElapsed,
  intersects,
  lifecycleState,
  liveActorVisible,
  liveTimelineRange,
} from "./actor_timeline_model.ts";


const ROW_HEIGHT = 116;
const SCENE_AREA_HEIGHT = 82;
const ACTOR_RAIL_OFFSET = 16;
const CUE_BAR_OFFSET = 31;
const ACT_BAR_OFFSET = 49;
const CUSTOM_SPAN_OFFSET = 73;
const CUSTOM_SPAN_DEPTH_STEP = 17;
const EVENT_MARKER_MIN_OFFSET = 108;
const EVENT_MARKER_SPAN_GAP = 17;
const EVENT_MARKER_HIT_RADIUS = 12;
const EVENT_MARKER_LANE_STEP = 28;
const CUE_BAR_HEIGHT = 11;
const CUE_LANE_STEP = 24;
const ACT_BAR_HEIGHT = 14;
const CUSTOM_SPAN_HEIGHT = 13;
const HISTORY_MIN_RANGE = 12;
const DEFAULT_LIVE_WINDOW_SECONDS = 10;
const SCENE_LABEL_CHAR_WIDTH = 5.8;
const SCENE_LABEL_GAP = 4;
const SCENE_COLORS: Readonly<Record<SceneRecord["tone"], string>> = {
  green: "#18765d",
  blue: "#356b8c",
  amber: "#ad641f",
  violet: "#745d86",
};

interface DisplayRow {
  readonly slot: number;
  readonly actor: ActorRecord | null;
  readonly pinned: boolean;
}

interface DisplayRowLayout extends DisplayRow {
  readonly top: number;
  readonly height: number;
  readonly cueLaneCount: number;
}

interface TimelineLayout {
  readonly rows: readonly DisplayRowLayout[];
  readonly rowByActor: ReadonlyMap<string, DisplayRowLayout>;
  readonly cueLaneById: ReadonlyMap<string, number>;
  readonly cues: readonly CueRecord[];
  readonly height: number;
}

function eventMarkerOffset(deepestSpanDepth: number | undefined): number {
  if (deepestSpanDepth === undefined) {
    return EVENT_MARKER_MIN_OFFSET;
  }
  return Math.max(
    EVENT_MARKER_MIN_OFFSET,
    CUSTOM_SPAN_OFFSET
      + deepestSpanDepth * CUSTOM_SPAN_DEPTH_STEP
      + CUSTOM_SPAN_HEIGHT
      + EVENT_MARKER_SPAN_GAP,
  );
}

type ActorDatumKind =
  | "actor_lifetime"
  | "actor_created"
  | "actor_destroyed"
  | "actor_failed"
  | "actor_active"
  | "actor_continuation";
type TimelineDatumKind = ActorDatumKind | "cue_wait" | "act" | "custom_span" | "custom_event" | SystemEventRecord["kind"];

interface TimelineTooltipState {
  readonly key: string;
  readonly kind: TimelineDatumKind;
  readonly label: string;
  readonly at: number;
  readonly end: number | null | undefined;
  readonly status: string;
  readonly actor: ActorRecord;
  readonly cue?: CueRecord | undefined;
  readonly actLabel?: string | undefined;
  readonly spanName?: string | undefined;
  readonly severity?: string | undefined;
  readonly attributes?: Readonly<Record<string, DiagnosticAttribute>> | undefined;
  readonly blockedBy?: readonly string[] | undefined;
  readonly anchorAt?: number | undefined;
  readonly anchorY: number;
}

interface IconButtonProps {
  readonly label: string;
  readonly onClick: () => void;
  readonly children: JSX.Element;
  readonly disabled?: boolean | undefined;
  readonly pressed?: boolean | undefined;
}

function IconButton({
  label,
  onClick,
  children,
  disabled = false,
  pressed,
}: IconButtonProps): JSX.Element {
  return (
    <button
      class="icon-button"
      type="button"
      title={label}
      aria-label={label}
      aria-pressed={pressed}
      disabled={disabled}
      onClick={onClick}
    >
      {children}
    </button>
  );
}

function ModeSwitch({
  mode,
  onChange,
}: {
  readonly mode: TimelineMode;
  readonly onChange: (mode: TimelineMode) => void;
}): JSX.Element {
  return (
    <div class="segmented" role="group" aria-label="Timeline mode">
      <button
        type="button"
        data-active={mode === "live"}
        aria-pressed={mode === "live"}
        onClick={() => onChange("live")}
      >
        <Radio aria-hidden="true" />
        Live
      </button>
      <button
        type="button"
        data-active={mode === "history"}
        aria-pressed={mode === "history"}
        onClick={() => onChange("history")}
      >
        <HistoryIcon aria-hidden="true" />
        History
      </button>
    </div>
  );
}

function stateLabel(state: LifecycleState): string {
  switch (state) {
    case "future":
      return "Not started";
    case "active":
      return "Active";
    case "completed":
      return "Completed";
    case "failed":
      return "Failed";
    case "cancelled":
      return "Cancelled";
  }
}

function stateOpacity(state: LifecycleState): number {
  switch (state) {
    case "future":
      return 0.24;
    case "active":
      return 1;
    case "completed":
      return 0.64;
    case "failed":
    case "cancelled":
      return 0.9;
  }
}

interface SceneLabelPlacement {
  readonly text: string;
  readonly x: number;
  readonly width: number;
}

function sceneLabelCandidate(
  label: string,
  barWidth: number,
): { readonly text: string; readonly width: number } | null {
  const estimate = (text: string): number => text.length * SCENE_LABEL_CHAR_WIDTH + 10;
  const fullWidth = estimate(label);
  if (barWidth >= fullWidth + SCENE_LABEL_GAP * 2) {
    return { text: label, width: fullWidth };
  }
  const shortLabel = label.replace(/^Scene\s+/i, "");
  const shortWidth = estimate(shortLabel);
  if (barWidth >= shortWidth + SCENE_LABEL_GAP * 2) {
    return { text: shortLabel, width: shortWidth };
  }
  const normalizedId = shortLabel.replace(/^scene-/i, "");
  const uuidPrefix = normalizedId.match(/^([0-9a-f]{8})[0-9a-f-]*$/i)?.[1];
  if (uuidPrefix !== undefined) {
    const compactLabel = `Scene ${uuidPrefix}`;
    const compactWidth = estimate(compactLabel);
    if (barWidth >= compactWidth + SCENE_LABEL_GAP * 2) {
      return { text: compactLabel, width: compactWidth };
    }
    const suffixLabel = `…${normalizedId.slice(-4)}`;
    const suffixWidth = estimate(suffixLabel);
    if (barWidth >= suffixWidth + SCENE_LABEL_GAP * 2) {
      return { text: suffixLabel, width: suffixWidth };
    }
  }
  return null;
}

function sceneLabelPlacements(
  scenes: readonly SceneRecord[],
  x: (time: number) => number,
  endAt: (scene: SceneRecord) => number,
): ReadonlyMap<string, SceneLabelPlacement> {
  const occupied: Array<{ readonly left: number; readonly right: number }> = [];
  const placements = new Map<string, SceneLabelPlacement>();
  for (const scene of scenes) {
    const startX = x(scene.start);
    const endX = x(endAt(scene));
    const candidate = sceneLabelCandidate(scene.label, endX - startX);
    if (candidate === null) {
      continue;
    }
    const minimumLeft = startX + SCENE_LABEL_GAP;
    const maximumLeft = endX - candidate.width - SCENE_LABEL_GAP;
    if (maximumLeft < minimumLeft) {
      continue;
    }
    const preferredLeft = (startX + endX - candidate.width) / 2;
    const choices = [
      preferredLeft,
      minimumLeft,
      maximumLeft,
    ];
    const left = choices.find((choice) => (
      choice >= minimumLeft
      && choice <= maximumLeft
      && occupied.every((interval) => (
        choice >= interval.right + SCENE_LABEL_GAP
        || choice + candidate.width <= interval.left - SCENE_LABEL_GAP
      ))
    ));
    if (left === undefined) {
      continue;
    }
    occupied.push({ left, right: left + candidate.width });
    placements.set(scene.id, { ...candidate, x: left });
  }
  return placements;
}

function eventKindLabel(kind: TimelineDatumKind): string {
  switch (kind) {
    case "actor_lifetime":
      return "Actor lifetime";
    case "actor_created":
      return "Actor created";
    case "actor_destroyed":
      return "Actor destroyed";
    case "actor_failed":
      return "Actor failed";
    case "actor_active":
      return "Actor active";
    case "actor_continuation":
      return "Actor continues";
    case "cue_wait":
      return "Cue wait";
    case "act":
      return "Act";
    case "custom_span":
      return "Python span";
    case "custom_event":
      return "Python event";
    case "tool":
      return "Tool call";
    case "message":
      return "Message";
  }
}

function eventStatusLabel(marker: SystemEventRecord, cursor: number): string {
  if (cursor < marker.at) {
    return "Pending";
  }
  return marker.outcome === null ? "Observed" : stateLabel(marker.outcome);
}

function EventGlyph({
  kind,
  x = 0,
  y = 0,
  size = 14,
}: {
  readonly kind: TimelineDatumKind;
  readonly x?: number | undefined;
  readonly y?: number | undefined;
  readonly size?: number | undefined;
}): JSX.Element {
  const iconProps = {
    class: "event-glyph",
    x,
    y,
    width: size,
    height: size,
    strokeWidth: 2.1,
    "aria-hidden": "true" as const,
  };
  switch (kind) {
    case "actor_lifetime":
    case "actor_active":
    case "actor_continuation":
      return <Activity {...iconProps} />;
    case "actor_created":
      return <UserRoundPlus {...iconProps} />;
    case "actor_destroyed":
      return <UserRoundCheck {...iconProps} />;
    case "actor_failed":
      return <UserRoundX {...iconProps} />;
    case "act":
      return <Activity {...iconProps} />;
    case "cue_wait":
      return <Clock3 {...iconProps} />;
    case "custom_span":
      return <Braces {...iconProps} />;
    case "custom_event":
      return <Code2 {...iconProps} />;
    case "tool":
      return <Wrench {...iconProps} />;
    case "message":
      return <MessageSquare {...iconProps} />;
  }
}

function isActorDatum(kind: TimelineDatumKind): kind is ActorDatumKind {
  switch (kind) {
    case "actor_lifetime":
    case "actor_created":
    case "actor_destroyed":
    case "actor_failed":
    case "actor_active":
    case "actor_continuation":
      return true;
    default:
      return false;
  }
}

function attributeValue(value: DiagnosticAttribute): string {
  return Array.isArray(value) ? value.join(", ") : String(value);
}

function attributeSummary(attributes: Readonly<Record<string, DiagnosticAttribute>>): string {
  return Object.entries(attributes)
    .map(([key, value]) => `${key}=${attributeValue(value)}`)
    .join(" · ");
}

function spanDepth(
  span: CustomSpanRecord,
  spansById: ReadonlyMap<string, CustomSpanRecord>,
  seen: ReadonlySet<string> = new Set(),
): number {
  if (span.parentSpanId === null || seen.has(span.id)) {
    return 0;
  }
  const parent = spansById.get(span.parentSpanId);
  if (parent === undefined) {
    return 0;
  }
  return 1 + spanDepth(parent, spansById, new Set([...seen, span.id]));
}

function sceneForId(data: TimelineData, sceneId: string): SceneRecord {
  const scene = data.scenes.find((candidate) => candidate.id === sceneId);
  if (scene === undefined) {
    throw new RangeError(`unknown production Scene: ${sceneId}`);
  }
  return scene;
}

function blockingActIds(
  cue: CueRecord,
  actorId: string,
  acts: readonly ActRecord[],
  cuesById: ReadonlyMap<string, CueRecord>,
  waitEnd: number,
): readonly string[] {
  return acts
    .filter((act) => {
      if (act.cueId === cue.id) {
        return false;
      }
      const actCue = cuesById.get(act.cueId);
      if (actCue?.actorId !== actorId) {
        return false;
      }
      const actEnd = act.end ?? waitEnd;
      return act.start < waitEnd && actEnd > cue.admitted;
    })
    .sort((left, right) => left.start - right.start)
    .map((act) => act.id);
}

function actorRows(
  data: TimelineData,
  mode: TimelineMode,
  range: TimelineRange,
  cursor: number,
  liveWindow: number,
  selectedActorId: string | null,
): readonly DisplayRow[] {
  if (mode === "history") {
    return data.actors
      .filter((actor) => intersects(actor.start, actor.end, range))
      .sort((left, right) => left.start - right.start)
      .map((actor, index) => ({ slot: index, actor, pinned: false }));
  }

  const regular = data.actors.filter((actor) => (
    liveActorVisible(actor, cursor, liveWindow, null)
  ));
  const bySlot = new Map<number, DisplayRow>();
  const selected = data.actors.find((actor) => actor.id === selectedActorId) ?? null;
  if (
    selected !== null
    && selected.end === null
    && selected.start <= cursor
    && !regular.some((actor) => actor.id === selected.id)
  ) {
    bySlot.set(selected.liveSlot, { slot: selected.liveSlot, actor: selected, pinned: true });
  }
  for (const actor of regular) {
    let slot = actor.liveSlot;
    if (bySlot.has(slot)) {
      slot = data.liveSlotCount ?? 6;
    }
    while (bySlot.has(slot)) {
      slot += 1;
    }
    bySlot.set(slot, { slot, actor, pinned: false });
  }
  const maximum = Math.max((data.liveSlotCount ?? 6) - 1, ...bySlot.keys());
  return Array.from({ length: maximum + 1 }, (_, slot) => (
    bySlot.get(slot) ?? { slot, actor: null, pinned: false }
  ));
}

function timeTicks(range: TimelineRange): readonly number[] {
  const duration = range.end - range.start;
  const step = duration <= 40 ? 10 : duration <= 90 ? 15 : 30;
  const first = Math.ceil(Math.max(0, range.start) / step) * step;
  const ticks: number[] = [];
  for (let value = first; value <= range.end; value += step) {
    ticks.push(value);
  }
  return ticks;
}

function buildTimelineLayout(
  data: TimelineData,
  mode: TimelineMode,
  range: TimelineRange,
  cursor: number,
  rows: readonly DisplayRow[],
): TimelineLayout {
  const visibleActorIds = new Set(
    rows.flatMap((row) => row.actor === null ? [] : [row.actor.id]),
  );
  const cues = data.cues.filter((cue) => (
    visibleActorIds.has(cue.actorId)
    && !(
      cue.lifecycleObserved === false
      && (cue.lastObserved ?? cue.admitted) < range.start
    )
    && intersects(cue.admitted, cue.end, range)
    && (mode === "history" || cue.admitted <= cursor)
  ));
  const cueLaneById = new Map<string, number>();
  const laneCountByActor = new Map<string, number>();

  for (const row of rows) {
    if (row.actor === null) {
      continue;
    }
    const actorId = row.actor.id;
    const laneEnds: number[] = [];
    const actorCues = cues
      .filter((cue) => cue.actorId === actorId)
      .sort((left, right) => (
        left.admitted - right.admitted
        || left.execution - right.execution
        || left.id.localeCompare(right.id)
      ));
    for (const cue of actorCues) {
      const intervalEnd = Math.max(
        cue.admitted,
        cue.end ?? (mode === "live" ? cursor : range.end),
      );
      let lane = laneEnds.findIndex((end) => end <= cue.admitted);
      if (lane === -1) {
        lane = laneEnds.length;
      }
      laneEnds[lane] = intervalEnd;
      cueLaneById.set(cue.id, lane);
    }
    laneCountByActor.set(actorId, Math.max(1, laneEnds.length));
  }

  const visibleCueIds = new Set(cues.map((cue) => cue.id));
  const spansById = new Map(data.customSpans.map((span) => [span.id, span]));
  const cueActorById = new Map(cues.map((cue) => [cue.id, cue.actorId]));
  const deepestSpanDepthByCue = new Map<string, number>();
  for (const span of data.customSpans) {
    deepestSpanDepthByCue.set(
      span.cueId,
      Math.max(deepestSpanDepthByCue.get(span.cueId) ?? 0, spanDepth(span, spansById)),
    );
  }
  const customBottomByActor = new Map<string, number>();
  const recordCustomBottom = (actorId: string, bottom: number): void => {
    customBottomByActor.set(
      actorId,
      Math.max(customBottomByActor.get(actorId) ?? 0, bottom),
    );
  };
  for (const span of data.customSpans) {
    const actorId = cueActorById.get(span.cueId);
    if (actorId === undefined || !intersects(span.start, span.end, range)) {
      continue;
    }
    if (mode === "live" && span.start > cursor) {
      continue;
    }
    const depth = spanDepth(span, spansById);
    recordCustomBottom(
      actorId,
      CUSTOM_SPAN_OFFSET + depth * CUSTOM_SPAN_DEPTH_STEP + CUSTOM_SPAN_HEIGHT + 5,
    );
  }
  const systemEventCueIds = new Set<string>();
  for (const cue of cues) {
    if (cue.events.some((event) => (
      event.at >= range.start
      && event.at <= range.end
      && (mode === "history" || event.at <= cursor)
    ))) {
      systemEventCueIds.add(cue.id);
      recordCustomBottom(
        cue.actorId,
        eventMarkerOffset(deepestSpanDepthByCue.get(cue.id)) + EVENT_MARKER_HIT_RADIUS,
      );
    }
  }
  for (const event of data.customEvents) {
    const actorId = cueActorById.get(event.cueId);
    if (
      actorId === undefined
      || !visibleCueIds.has(event.cueId)
      || event.at < range.start
      || event.at > range.end
      || (mode === "live" && event.at > cursor)
    ) {
      continue;
    }
    // Keep event hit targets below the deepest custom span so the two hover
    // surfaces remain independently usable when Python events are nested.
    recordCustomBottom(
      actorId,
      eventMarkerOffset(deepestSpanDepthByCue.get(event.cueId))
        + (systemEventCueIds.has(event.cueId) ? EVENT_MARKER_LANE_STEP : 0)
        + EVENT_MARKER_HIT_RADIUS,
    );
  }

  let top = SCENE_AREA_HEIGHT;
  const layoutRows = rows.map((row): DisplayRowLayout => {
    const cueLaneCount = row.actor === null
      ? 1
      : laneCountByActor.get(row.actor.id) ?? 1;
    const extraCueHeight = (cueLaneCount - 1) * CUE_LANE_STEP;
    const customHeight = row.actor === null
      ? 0
      : customBottomByActor.get(row.actor.id) ?? 0;
    const height = Math.max(ROW_HEIGHT, customHeight) + extraCueHeight;
    const layoutRow = { ...row, top, height, cueLaneCount };
    top += height;
    return layoutRow;
  });
  const rowByActor = new Map<string, DisplayRowLayout>();
  for (const row of layoutRows) {
    if (row.actor !== null) {
      rowByActor.set(row.actor.id, row);
    }
  }

  return {
    rows: layoutRows,
    rowByActor,
    cueLaneById,
    cues,
    height: top + 12,
  };
}

function TimelinePlot({
  data,
  mode,
  range,
  cursor,
  layout,
  selectedActorId,
  onSelectActor,
}: {
  readonly data: TimelineData;
  readonly mode: TimelineMode;
  readonly range: TimelineRange;
  readonly cursor: number;
  readonly layout: TimelineLayout;
  readonly selectedActorId: string | null;
  readonly onSelectActor: (actorId: string) => void;
}): JSX.Element {
  const host = useRef<HTMLDivElement | null>(null);
  const [measuredWidth, setMeasuredWidth] = useState(880);
  const [hoveredDatum, setHoveredDatum] = useState<TimelineTooltipState | null>(null);
  useLayoutEffect(() => {
    const element = host.current;
    if (element === null) {
      return;
    }
    const update = (): void => setMeasuredWidth(Math.max(720, element.clientWidth));
    update();
    if (typeof ResizeObserver === "undefined") {
      return;
    }
    const observer = new ResizeObserver(update);
    observer.observe(element);
    return () => observer.disconnect();
  }, []);

  const width = measuredWidth;
  const padding = 14;
  const innerWidth = width - padding * 2;
  const { cues, cueLaneById, height, rowByActor, rows } = layout;
  const duration = Math.max(1, range.end - range.start);
  const x = (time: number): number => (
    padding + ((Math.max(range.start, Math.min(range.end, time)) - range.start) / duration) * innerWidth
  );
  const scenes = data.scenes.filter((scene) => (
    intersects(scene.start, scene.end, range) && (mode === "history" || scene.start <= cursor)
  ));
  const sceneLabelById = sceneLabelPlacements(
    scenes,
    x,
    (scene) => Math.min(scene.end ?? (mode === "live" ? cursor : range.end), range.end),
  );
  const visibleCueIds = new Set(cues.map((cue) => cue.id));
  const acts = data.acts.filter((act) => (
    visibleCueIds.has(act.cueId)
    && intersects(act.start, act.end, range)
    && (mode === "history" || act.start <= cursor)
  ));
  const actById = new Map(acts.map((act) => [act.id, act]));
  const cueById = new Map(cues.map((cue) => [cue.id, cue]));
  const customSpans = data.customSpans.filter((span) => (
    visibleCueIds.has(span.cueId)
    && intersects(span.start, span.end, range)
    && (mode === "history" || span.start <= cursor)
  ));
  const spanById = new Map(customSpans.map((span) => [span.id, span]));
  const customEvents = data.customEvents.filter((event) => (
    visibleCueIds.has(event.cueId)
    && event.at >= range.start
    && event.at <= range.end
    && (mode === "history" || event.at <= cursor)
  ));
  const ticks = timeTicks(range);
  const cursorX = x(cursor);
  const hoveredRow = hoveredDatum === null
    ? undefined
    : rowByActor.get(hoveredDatum.actor.id);
  const tooltipPoint = hoveredDatum === null || hoveredRow === undefined
    ? null
    : {
        x: x(hoveredDatum.anchorAt ?? hoveredDatum.at),
        y: hoveredDatum.anchorY,
      };
  const tooltipWidth = 236;
  const tooltipHeight = hoveredDatum?.kind === "custom_event" ? 244 : 210;
  const tooltipPosition = tooltipPoint === null
    ? null
    : {
        left: Math.max(8, Math.min(width - tooltipWidth - 8, tooltipPoint.x + 12)),
        top: tooltipPoint.y + 18 + tooltipHeight <= height
          ? tooltipPoint.y + 18
          : Math.max(8, tooltipPoint.y - tooltipHeight - 12),
      };
  const hideTooltip = (key: string): void => {
    setHoveredDatum((current) => current?.key === key ? null : current);
  };
  const tooltipDetails: readonly (readonly [string, string])[] = hoveredDatum === null
    ? []
    : [
        ["Status", hoveredDatum.status],
        ...(hoveredDatum.end === undefined ? [] : [[
          "Duration",
          hoveredDatum.end === null
            ? "Open"
            : `${Math.max(0, hoveredDatum.end - hoveredDatum.at).toFixed(1)} s`,
        ] as const]),
        ["Actor", hoveredDatum.actor.name],
        ...(isActorDatum(hoveredDatum.kind)
          ? [["Role", hoveredDatum.actor.role] as const]
          : []),
        ...(hoveredDatum.cue === undefined ? [] : [["Cue", hoveredDatum.cue.label] as const]),
        ...(hoveredDatum.actLabel === undefined || hoveredDatum.kind === "act"
          ? []
          : [["Act", hoveredDatum.actLabel] as const]),
        ...(hoveredDatum.spanName === undefined
          ? []
          : [[hoveredDatum.kind === "custom_span" ? "Parent" : "Span", hoveredDatum.spanName] as const]),
        ...(hoveredDatum.severity === undefined ? [] : [["Severity", hoveredDatum.severity] as const]),
        ...(hoveredDatum.attributes === undefined || Object.keys(hoveredDatum.attributes).length === 0
          ? []
          : [["Attributes", attributeSummary(hoveredDatum.attributes)] as const]),
        ...(hoveredDatum.kind === "cue_wait"
          ? [[
            "Queue",
            hoveredDatum.blockedBy !== undefined && hoveredDatum.blockedBy.length > 0
              ? `Queued behind ${hoveredDatum.blockedBy.join(", ")}`
              : "Actor mailbox",
          ] as const]
          : []),
        ...(hoveredDatum.cue === undefined
          ? []
          : [["Scene", sceneForId(data, hoveredDatum.cue.sceneId).label] as const]),
      ];

  return (
    <div class="timeline-plot" ref={host}>
      <svg
        width="100%"
        height={height}
        viewBox={`0 0 ${width} ${height}`}
        role="group"
        aria-label={`${mode === "live" ? "Live" : "History"} Actor timeline from ${formatElapsed(range.start)} to ${formatElapsed(range.end)}`}
      >
        <rect class="plot-background" x="0" y="0" width={width} height={height} />

        {scenes.map((scene) => {
          const end = Math.min(scene.end ?? (mode === "live" ? cursor : range.end), range.end);
          const startX = x(scene.start);
          const endX = x(end);
          const color = SCENE_COLORS[scene.tone];
          const sceneState = actorState(
            {
              id: scene.id,
              name: scene.label,
              role: "Scene",
              start: scene.start,
              end: scene.end,
              outcome: scene.outcome,
              liveSlot: 0,
            },
            cursor,
          );
          return (
            <g
              key={scene.id}
              class="scene-band"
              data-scene-id={scene.id}
              opacity={stateOpacity(sceneState)}
            >
              <rect
                x={startX}
                y={SCENE_AREA_HEIGHT - 12}
                width={Math.max(2, endX - startX)}
                height={height - SCENE_AREA_HEIGHT + 2}
                fill={color}
                opacity="0.055"
              />
              <rect
                x={startX}
                y="40"
                width={Math.max(3, endX - startX)}
                height="13"
                rx="2"
                fill={color}
              />
              {sceneLabelById.has(scene.id) ? (
                <text
                  x={sceneLabelById.get(scene.id)!.x}
                  y="33"
                  fill={color}
                  class="scene-label-svg"
                >
                  {sceneLabelById.get(scene.id)!.text}
                </text>
              ) : null}
              <title>{`${scene.label}: ${formatElapsed(scene.start)} to ${scene.end === null ? "open" : formatElapsed(scene.end)}`}</title>
            </g>
          );
        })}

        {ticks.map((tick) => (
          <g key={tick}>
            <line
              class="time-grid-line"
              x1={x(tick)}
              x2={x(tick)}
              y1="20"
              y2={height}
            />
            <text class="time-label-svg" x={x(tick) + 4} y="15">
              {formatElapsed(tick)}
            </text>
          </g>
        ))}

        {rows.map((row, index) => {
          return (
            <g key={`row-${row.slot}`}>
              <rect
                x="0"
                y={row.top}
                width={width}
                height={row.height}
                fill={index % 2 === 0 ? "#fbfcfb" : "#f4f6f4"}
              />
              <line
                class="row-rule"
                x1="0"
                x2={width}
                y1={row.top + row.height}
                y2={row.top + row.height}
              />
            </g>
          );
        })}

        {cues.map((cue) => {
          const row = rowByActor.get(cue.actorId)!;
          const actorY = row.top + ACTOR_RAIL_OFFSET;
          const cueX = x(cue.admitted);
          const selected = cue.actorId === selectedActorId;
          const scene = sceneForId(data, cue.sceneId);
          const cueVisualState = cueState(cue, cursor);
          return (
            <path
              key={`connector-${cue.id}`}
              d={`M ${cueX} 54 V ${actorY - 5}`}
              fill="none"
              stroke={SCENE_COLORS[scene.tone]}
              strokeWidth={selected ? 1.5 : 1}
              strokeDasharray={selected ? "0" : "2 4"}
              opacity={selected ? 0.65 : 0.14 * stateOpacity(cueVisualState)}
            />
          );
        })}

        {rows.map((row) => {
          const actor = row.actor;
          if (actor === null) {
            return null;
          }
          const rowTop = row.top;
          const extraCueTrackHeight = (row.cueLaneCount - 1) * CUE_LANE_STEP;
          const actBarY = rowTop + ACT_BAR_OFFSET + extraCueTrackHeight;
          const y = rowTop + ACTOR_RAIL_OFFSET;
          const state = actorState(actor, cursor);
          const opacity = stateOpacity(state);
          const selected = actor.id === selectedActorId;
          const start = Math.max(actor.start, range.start);
          const naturalEnd = actor.end ?? (mode === "live" ? cursor : range.end);
          const renderedEnd = mode === "live"
            ? Math.min(naturalEnd, cursor, range.end)
            : Math.min(naturalEnd, range.end);
          const startX = x(start);
          const endX = x(Math.max(start, renderedEnd));
          const actorCues = cues.filter((cue) => cue.actorId === actor.id);
          const color = actor.outcome === "failed" && state === "failed" ? "#b42318" : "#34483f";
          const lifetimeWidth = Math.max(0, endX - startX);
          const fullLifetimeLabel = `${actor.name} Actor lifetime`;
          const lifetimeLabel = fullLifetimeLabel;
          const lifetimeLabelCompact = lifetimeWidth < 14 + fullLifetimeLabel.length * 5.1;
          const lifetimeLabelPlacement = state === "active" && !lifetimeLabelCompact ? "end" : "start";
          const lifetimeKey = `${actor.id}:lifetime`;
          const lifetimeTooltip: TimelineTooltipState = {
            key: lifetimeKey,
            kind: "actor_lifetime",
            label: fullLifetimeLabel,
            at: actor.start,
            end: actor.end,
            status: stateLabel(state),
            actor,
            anchorAt: (start + Math.max(start, renderedEnd)) / 2,
            anchorY: y,
          };
          const createdKey = `${actor.id}:created`;
          const createdStatus = cursor < actor.start ? "Pending" : "Observed";
          const createdTooltip: TimelineTooltipState = {
            key: createdKey,
            kind: "actor_created",
            label: actor.name,
            at: actor.start,
            end: undefined,
            status: createdStatus,
            actor,
            anchorAt: start,
            anchorY: y,
          };
          const terminalInRange = actor.end !== null && actor.end <= range.end;
          const boundaryKind: ActorDatumKind = terminalInRange
            ? actor.outcome === "failed" ? "actor_failed" : "actor_destroyed"
            : state === "active" ? "actor_active" : "actor_continuation";
          const boundaryAt = terminalInRange
            ? actor.end!
            : Math.max(start, renderedEnd);
          const boundaryStatus = terminalInRange
            ? cursor < boundaryAt ? "Pending" : stateLabel(actor.outcome ?? "completed")
            : stateLabel(state);
          const boundaryEvent = boundaryKind.replace("actor_", "");
          const boundaryKey = `${actor.id}:${boundaryEvent}`;
          const boundaryTooltip: TimelineTooltipState = {
            key: boundaryKey,
            kind: boundaryKind,
            label: actor.name,
            at: boundaryAt,
            end: undefined,
            status: boundaryStatus,
            actor,
            anchorY: y,
          };
          return (
            <g
              key={actor.id}
              class="actor-visual"
              opacity={opacity}
              data-selected={selected}
              data-cue-lanes={row.cueLaneCount}
              data-actor-id={actor.id}
              data-row-top={rowTop}
              data-row-height={row.height}
              onClick={() => onSelectActor(actor.id)}
            >
              {selected ? (
                <rect
                  x="2"
                  y={rowTop + 3}
                  width={width - 4}
                  height={row.height - 6}
                  fill="#e5f1ee"
                  opacity="0.72"
                />
              ) : null}
              <g
                class="actor-lifetime-track"
                data-actor-id={actor.id}
                role="img"
                tabIndex={0}
                aria-label={`${fullLifetimeLabel}, ${formatElapsed(actor.start)} to ${actor.end === null ? "open" : formatElapsed(actor.end)}, ${stateLabel(state)}`}
                aria-describedby={hoveredDatum?.key === lifetimeKey ? "timeline-event-tooltip" : undefined}
                onMouseEnter={() => setHoveredDatum(lifetimeTooltip)}
                onMouseLeave={() => hideTooltip(lifetimeKey)}
                onFocus={() => setHoveredDatum(lifetimeTooltip)}
                onBlur={() => hideTooltip(lifetimeKey)}
              >
                <line
                  class="actor-lifetime-hit"
                  x1={startX}
                  x2={endX}
                  y1={y}
                  y2={y}
                />
                <line
                  class="actor-lifetime-line"
                  x1={startX}
                  x2={endX}
                  y1={y}
                  y2={y}
                  stroke={color}
                  strokeWidth={selected ? 3.5 : 2.5}
                />
                <text
                  class="actor-lifetime-label-svg"
                  data-placement={lifetimeLabelPlacement}
                  data-compact={lifetimeLabelCompact}
                  x={lifetimeLabelPlacement === "end" ? endX - 10 : startX + 10}
                  y={y - 5}
                  fill={color}
                >
                  {lifetimeLabel}
                </text>
              </g>
              <g
                class="actor-lifecycle-marker"
                data-actor-id={actor.id}
                data-event="created"
                data-clipped={actor.start < range.start}
                role="img"
                tabIndex={0}
                aria-label={`Actor created: ${actor.name}, ${formatElapsed(actor.start)}, ${createdStatus}`}
                aria-describedby={hoveredDatum?.key === createdKey ? "timeline-event-tooltip" : undefined}
                onMouseEnter={() => setHoveredDatum(createdTooltip)}
                onMouseLeave={() => hideTooltip(createdKey)}
                onFocus={() => setHoveredDatum(createdTooltip)}
                onBlur={() => hideTooltip(createdKey)}
              >
                <rect
                  class="actor-lifecycle-marker__hit"
                  x={startX - 12}
                  y={y - 12}
                  width="24"
                  height="24"
                />
                <rect
                  class="actor-lifecycle-marker__focus"
                  x={startX - 9}
                  y={y - 9}
                  width="18"
                  height="18"
                  rx="3"
                  stroke={color}
                />
                {actor.start < range.start ? (
                  <polygon
                    points={`${padding},${y} ${padding + 7},${y - 5} ${padding + 7},${y + 5}`}
                    fill={color}
                  />
                ) : (
                  <circle
                    class="actor-start-marker"
                    cx={startX}
                    cy={y}
                    r="5"
                    fill="#ffffff"
                    stroke={color}
                    strokeWidth="2"
                  />
                )}
              </g>
              <g
                class="actor-lifecycle-marker"
                data-actor-id={actor.id}
                data-event={boundaryEvent}
                role="img"
                tabIndex={0}
                aria-label={`${eventKindLabel(boundaryKind)}: ${actor.name}, ${formatElapsed(boundaryAt)}, ${boundaryStatus}`}
                aria-describedby={hoveredDatum?.key === boundaryKey ? "timeline-event-tooltip" : undefined}
                onMouseEnter={() => setHoveredDatum(boundaryTooltip)}
                onMouseLeave={() => hideTooltip(boundaryKey)}
                onFocus={() => setHoveredDatum(boundaryTooltip)}
                onBlur={() => hideTooltip(boundaryKey)}
              >
                <rect
                  class="actor-lifecycle-marker__hit"
                  x={endX - 12}
                  y={y - 12}
                  width="24"
                  height="24"
                />
                <rect
                  class="actor-lifecycle-marker__focus"
                  x={endX - 9}
                  y={y - 9}
                  width="18"
                  height="18"
                  rx="3"
                  stroke={color}
                />
                {terminalInRange ? (
                  actor.outcome === "failed" && cursor >= actor.end! ? (
                    <g stroke="#b42318" strokeWidth="2.5">
                      <line x1={endX - 5} x2={endX + 5} y1={y - 5} y2={y + 5} />
                      <line x1={endX - 5} x2={endX + 5} y1={y + 5} y2={y - 5} />
                    </g>
                  ) : (
                    <rect x={endX - 4} y={y - 4} width="8" height="8" fill={color} />
                  )
                ) : (
                  <polygon
                    points={`${endX},${y - 5} ${endX + 8},${y} ${endX},${y + 5}`}
                    fill={color}
                    class={state === "active" ? "open-marker" : undefined}
                  />
                )}
              </g>

              {actorCues.map((cue) => {
                const cueVisualState = cueState(cue, cursor);
                const cueOpacity = stateOpacity(cueVisualState);
                const cueLane = cueLaneById.get(cue.id) ?? 0;
                const cueBarY = rowTop + CUE_BAR_OFFSET + cueLane * CUE_LANE_STEP;
                const waitStartX = x(cue.admitted);
                const executionX = x(cue.execution);
                const executionStarted = mode === "history" || cursor >= cue.execution;
                const waitEndX = executionStarted ? executionX : x(cursor);
                const waitWidth = Math.max(0, waitEndX - waitStartX);
                const waitEndTime = executionStarted ? cue.execution : cursor;
                const blockedBy = blockingActIds(cue, actor.id, acts, cueById, waitEndTime);
                const cueEnd = cue.end ?? (mode === "live" ? cursor : range.end);
                const cueEndX = x(mode === "live" ? Math.min(cueEnd, cursor) : cueEnd);
                const executionWidth = Math.max(3, cueEndX - executionX);
                const cueSpanWidth = Math.max(waitWidth, cueEndX - waitStartX);
                const fullSendLabelWidth = 58 + cue.label.length * 5;
                const sendLabel = cueSpanWidth >= fullSendLabelWidth
                  ? `Cue sent · ${cue.label}`
                  : cueSpanWidth >= 54 ? "Cue sent" : null;
                const waitLabelWidth = 58 + cue.id.length * 4.6;
                const waitLabel = waitWidth >= waitLabelWidth
                  ? `Cue wait · ${cue.id}`
                  : waitWidth >= 42 ? "Cue wait" : null;
                const cueColor = cue.outcome === "failed" && cursor >= (cue.end ?? data.totalTime)
                  ? "#b42318"
                  : SCENE_COLORS[sceneForId(data, cue.sceneId).tone];
                const cueActs = acts.filter((act) => act.cueId === cue.id);
                const cueSpans = customSpans.filter((span) => span.cueId === cue.id);
                const cueCustomEvents = customEvents.filter((event) => event.cueId === cue.id);
                const deepestCueSpanDepth = cueSpans.length === 0
                  ? undefined
                  : Math.max(...cueSpans.map((span) => spanDepth(span, spanById)));
                const eventMarkerBaseY = rowTop
                  + eventMarkerOffset(deepestCueSpanDepth)
                  + extraCueTrackHeight;
                const hasSystemEvents = cue.events.some((event) => (
                  event.at >= range.start
                  && event.at <= range.end
                  && (mode === "history" || event.at <= cursor)
                ));
                const waitKey = `${cue.id}:wait`;
                const waitStatus = cursor < cue.admitted
                  ? "Pending"
                  : executionStarted ? "Completed" : "Active";
                const waitLabelText = `Cue wait · ${cue.id}`;
                const waitTooltip: TimelineTooltipState = {
                  key: waitKey,
                  kind: "cue_wait",
                  label: waitLabelText,
                  at: cue.admitted,
                  end: executionStarted ? cue.execution : null,
                  status: waitStatus,
                  actor,
                  cue,
                  blockedBy,
                  anchorAt: (cue.admitted + waitEndTime) / 2,
                  anchorY: cueBarY + CUE_BAR_HEIGHT / 2,
                };
                return (
                  <g
                    class="cue-track"
                    data-cue-id={cue.id}
                    data-cue-lane={cueLane}
                    key={cue.id}
                    opacity={cueOpacity}
                  >
                    {waitWidth >= 2 ? (
                      <g
                        class="cue-wait-track"
                        data-cue-id={cue.id}
                        data-blocked-by={blockedBy.join(",") || undefined}
                        role="img"
                        tabIndex={0}
                        aria-label={`${waitLabelText}: ${formatElapsed(cue.admitted)} to ${executionStarted ? formatElapsed(cue.execution) : "open"}, ${waitStatus}${blockedBy.length > 0 ? `, queued behind ${blockedBy.join(", ")}` : ""}`}
                        aria-describedby={hoveredDatum?.key === waitKey ? "timeline-event-tooltip" : undefined}
                        onMouseEnter={() => setHoveredDatum(waitTooltip)}
                        onMouseLeave={() => hideTooltip(waitKey)}
                        onFocus={() => setHoveredDatum(waitTooltip)}
                        onBlur={() => hideTooltip(waitKey)}
                      >
                        <rect
                          class="cue-wait-hit"
                          x={waitStartX}
                          y={cueBarY - 5}
                          width={Math.max(24, waitWidth)}
                          height={CUE_BAR_HEIGHT + 10}
                        />
                        <rect
                          class="cue-wait-bar"
                          x={waitStartX}
                          y={cueBarY}
                          width={waitWidth}
                          height={CUE_BAR_HEIGHT}
                          rx="2"
                        />
                        {waitLabel === null ? null : (
                          <text
                            class="cue-wait-label-svg"
                            x={waitStartX + waitWidth / 2}
                            y={cueBarY + 8}
                            textAnchor="middle"
                          >
                            {waitLabel}
                          </text>
                        )}
                      </g>
                    ) : null}
                    {cue.admitted >= range.start ? (
                      <>
                        <line
                          class="cue-admission-marker"
                          x1={waitStartX}
                          x2={waitStartX}
                          y1={cueBarY - 2}
                          y2={cueBarY + CUE_BAR_HEIGHT + 2}
                          stroke={cueColor}
                          strokeWidth="2"
                        />
                        {sendLabel === null ? null : (
                          <text
                            class="cue-send-label-svg"
                            x={waitStartX + 4}
                            y={cueBarY - 4}
                            fill={cueColor}
                          >
                            {sendLabel}
                          </text>
                        )}
                      </>
                    ) : null}
                    {executionStarted ? (
                      <>
                        <rect
                          class="cue-execution-bar"
                          x={executionX}
                          y={cueBarY}
                          width={executionWidth}
                          height={CUE_BAR_HEIGHT}
                          rx="2"
                          fill={cueColor}
                        />
                        {executionWidth >= 38 ? (
                          <text
                            class="cue-execution-label-svg"
                            x={executionX + executionWidth / 2}
                            y={cueBarY + 8}
                            textAnchor="middle"
                          >
                            Execute
                          </text>
                        ) : null}
                      </>
                    ) : null}
                    {cueActs.map((act) => {
                      const actVisualState = lifecycleState(act.start, act.end, act.outcome, cursor);
                      const actStartX = x(Math.max(act.start, range.start));
                      const naturalActEnd = act.end ?? (mode === "live" ? cursor : range.end);
                      const renderedActEnd = mode === "live"
                        ? Math.min(naturalActEnd, cursor, range.end)
                        : Math.min(naturalActEnd, range.end);
                      const actEndX = x(Math.max(act.start, renderedActEnd));
                      const actWidth = Math.max(3, actEndX - actStartX);
                      const actLabel = actWidth >= 58 + act.label.length * 4.6
                        ? `Act · ${act.label}`
                        : actWidth >= 30 ? "Act" : null;
                      const tooltipState: TimelineTooltipState = {
                        key: act.id,
                        kind: "act",
                        label: act.label,
                        at: act.start,
                        end: act.end,
                        status: stateLabel(actVisualState),
                        actor,
                        cue,
                        actLabel: act.label,
                        anchorY: actBarY + ACT_BAR_HEIGHT / 2,
                      };
                      return (
                        <g
                          key={act.id}
                          class="duration-track act-track"
                          data-act-id={act.id}
                          data-cue-id={cue.id}
                          data-outcome={act.outcome}
                          opacity={stateOpacity(actVisualState)}
                          role="img"
                          tabIndex={0}
                          aria-label={`Act: ${act.label}, ${formatElapsed(act.start)} to ${act.end === null ? "open" : formatElapsed(act.end)}, ${stateLabel(actVisualState)}`}
                          aria-describedby={hoveredDatum?.key === act.id ? "timeline-event-tooltip" : undefined}
                          onMouseEnter={() => setHoveredDatum(tooltipState)}
                          onMouseLeave={() => hideTooltip(act.id)}
                          onFocus={() => setHoveredDatum(tooltipState)}
                          onBlur={() => hideTooltip(act.id)}
                        >
                          <line
                            class="scope-connector"
                            x1={actStartX}
                            x2={actStartX}
                            y1={cueBarY + CUE_BAR_HEIGHT}
                            y2={actBarY}
                          />
                          <rect
                            class="duration-hit"
                            x={actStartX}
                            y={actBarY - 5}
                            width={Math.max(24, actWidth)}
                            height="24"
                          />
                          <rect
                            class="act-bar"
                            x={actStartX}
                            y={actBarY}
                            width={actWidth}
                            height={ACT_BAR_HEIGHT}
                            rx="2"
                          />
                          {actLabel === null ? null : (
                            <text
                              class="act-label-svg"
                              x={actStartX + actWidth / 2}
                              y={actBarY + 10}
                              textAnchor="middle"
                            >
                              {actLabel}
                            </text>
                          )}
                        </g>
                      );
                    })}
                    {cueSpans.map((span) => {
                      const depth = spanDepth(span, spanById);
                      const spanY = rowTop + CUSTOM_SPAN_OFFSET + extraCueTrackHeight
                        + depth * CUSTOM_SPAN_DEPTH_STEP;
                      const spanVisualState = lifecycleState(span.start, span.end, span.outcome, cursor);
                      const spanStartX = x(Math.max(span.start, range.start));
                      const naturalSpanEnd = span.end ?? (mode === "live" ? cursor : range.end);
                      const renderedSpanEnd = mode === "live"
                        ? Math.min(naturalSpanEnd, cursor, range.end)
                        : Math.min(naturalSpanEnd, range.end);
                      const spanEndX = x(Math.max(span.start, renderedSpanEnd));
                      const spanWidth = Math.max(3, spanEndX - spanStartX);
                      const spanNameParts = span.name.split(".");
                      const leafSpanName = spanNameParts[spanNameParts.length - 1] ?? span.name;
                      const fullSpanLabel = `span · ${span.name}`;
                      const leafSpanLabel = `span · ${leafSpanName}`;
                      const fullSpanLabelWidth = 12 + fullSpanLabel.length * 4.8;
                      const leafSpanLabelWidth = 10 + leafSpanLabel.length * 4.4;
                      const spanLabel = spanWidth >= fullSpanLabelWidth
                        ? fullSpanLabel
                        : spanWidth >= leafSpanLabelWidth
                          ? leafSpanLabel
                          : spanWidth >= 34 ? "span" : null;
                      const parent = span.parentSpanId === null ? undefined : spanById.get(span.parentSpanId);
                      const parentY = parent === undefined
                        ? actBarY + ACT_BAR_HEIGHT
                        : rowTop + CUSTOM_SPAN_OFFSET + extraCueTrackHeight
                          + spanDepth(parent, spanById) * CUSTOM_SPAN_DEPTH_STEP
                          + CUSTOM_SPAN_HEIGHT;
                      const act = span.actId === null ? undefined : actById.get(span.actId);
                      const tooltipState: TimelineTooltipState = {
                        key: span.id,
                        kind: "custom_span",
                        label: span.name,
                        at: span.start,
                        end: span.end,
                        status: stateLabel(spanVisualState),
                        actor,
                        cue,
                        actLabel: act?.label,
                        spanName: parent?.name,
                        attributes: span.attributes,
                        anchorY: spanY + CUSTOM_SPAN_HEIGHT / 2,
                      };
                      return (
                        <g
                          key={span.id}
                          class="duration-track custom-span-track"
                          data-span-id={span.id}
                          data-parent-span-id={span.parentSpanId ?? undefined}
                          data-act-id={span.actId ?? undefined}
                          data-outcome={span.outcome}
                          opacity={stateOpacity(spanVisualState)}
                          role="img"
                          tabIndex={0}
                          aria-label={`Python span: ${span.name}, ${formatElapsed(span.start)} to ${span.end === null ? "open" : formatElapsed(span.end)}, ${stateLabel(spanVisualState)}`}
                          aria-describedby={hoveredDatum?.key === span.id ? "timeline-event-tooltip" : undefined}
                          onMouseEnter={() => setHoveredDatum(tooltipState)}
                          onMouseLeave={() => hideTooltip(span.id)}
                          onFocus={() => setHoveredDatum(tooltipState)}
                          onBlur={() => hideTooltip(span.id)}
                        >
                          <line
                            class="scope-connector"
                            x1={spanStartX}
                            x2={spanStartX}
                            y1={parentY}
                            y2={spanY}
                          />
                          <rect
                            class="duration-hit"
                            x={spanStartX}
                            y={spanY - 5}
                            width={Math.max(24, spanWidth)}
                            height="23"
                          />
                          <rect
                            class="custom-span-bar"
                            x={spanStartX}
                            y={spanY}
                            width={spanWidth}
                            height={CUSTOM_SPAN_HEIGHT}
                            rx="2"
                          />
                          {spanLabel === null ? null : (
                            <text
                              class="custom-span-label-svg"
                              x={spanStartX + 5}
                              y={spanY + 9}
                            >
                              {spanLabel}
                            </text>
                          )}
                        </g>
                      );
                    })}
                    {cue.events
                      .filter((event) => event.at >= range.start && event.at <= range.end)
                      .filter((event) => mode === "history" || event.at <= cursor)
                      .map((event) => {
                        const markerX = x(event.at);
                        const markerY = eventMarkerBaseY;
                        const act = actById.get(event.actId);
                        const markerStatus = eventStatusLabel(event, cursor);
                        const tooltipState: TimelineTooltipState = {
                          key: event.id,
                          kind: event.kind,
                          label: event.label,
                          at: event.at,
                          end: undefined,
                          status: markerStatus,
                          actor,
                          cue,
                          actLabel: act?.label,
                          anchorY: markerY,
                        };
                        return (
                          <g
                            key={event.id}
                            class="event-marker"
                            data-kind={event.kind}
                            data-outcome={event.outcome}
                            data-event-label={event.label}
                            data-act-id={event.actId}
                            role="img"
                            tabIndex={0}
                            aria-label={`${eventKindLabel(event.kind)}: ${event.label}, ${formatElapsed(event.at)}, ${markerStatus}`}
                            aria-describedby={hoveredDatum?.key === event.id ? "timeline-event-tooltip" : undefined}
                            onMouseEnter={() => setHoveredDatum(tooltipState)}
                            onMouseLeave={() => hideTooltip(event.id)}
                            onFocus={() => setHoveredDatum(tooltipState)}
                            onBlur={() => hideTooltip(event.id)}
                          >
                            <rect
                              class="event-marker__hit"
                              x={markerX - EVENT_MARKER_HIT_RADIUS}
                              y={markerY - EVENT_MARKER_HIT_RADIUS}
                              width={EVENT_MARKER_HIT_RADIUS * 2}
                              height={EVENT_MARKER_HIT_RADIUS * 2}
                            />
                            <line
                              class="event-marker__anchor"
                              x1={markerX}
                              x2={markerX}
                              y1={actBarY + ACT_BAR_HEIGHT}
                              y2={markerY - 5}
                            />
                            <rect
                              class="event-marker__focus"
                              x={markerX - 8}
                              y={markerY - 8}
                              width="16"
                              height="16"
                              rx="3"
                            />
                            <EventGlyph kind={event.kind} x={markerX - 5} y={markerY - 5} size={10} />
                          </g>
                        );
                      })}
                    {cueCustomEvents.map((event) => {
                      const containingSpan = event.containingSpanId === null
                        ? undefined
                        : spanById.get(event.containingSpanId);
                      const act = event.actId === null ? undefined : actById.get(event.actId);
                      const eventY = eventMarkerBaseY
                        + (hasSystemEvents ? EVENT_MARKER_LANE_STEP : 0);
                      const eventScopeY = containingSpan === undefined
                        ? act === undefined
                          ? cueBarY + CUE_BAR_HEIGHT
                          : actBarY + ACT_BAR_HEIGHT
                        : rowTop + CUSTOM_SPAN_OFFSET + extraCueTrackHeight
                          + spanDepth(containingSpan, spanById) * CUSTOM_SPAN_DEPTH_STEP
                          + CUSTOM_SPAN_HEIGHT;
                      const markerX = x(event.at);
                      const eventStatus = cursor < event.at ? "Pending" : "Observed";
                      const tooltipState: TimelineTooltipState = {
                        key: event.id,
                        kind: "custom_event",
                        label: event.name,
                        at: event.at,
                        end: undefined,
                        status: eventStatus,
                        actor,
                        cue,
                        actLabel: act?.label,
                        spanName: containingSpan?.name,
                        severity: event.severity ?? "unspecified",
                        attributes: event.attributes,
                        anchorY: eventY,
                      };
                      return (
                        <g
                          key={event.id}
                          class="event-marker custom-event-marker"
                          data-kind="custom_event"
                          data-severity={event.severity ?? "unspecified"}
                          data-event-label={event.name}
                          data-containing-span-id={event.containingSpanId ?? undefined}
                          role="img"
                          tabIndex={0}
                          aria-label={`Python event: ${event.name}, ${formatElapsed(event.at)}, ${eventStatus}`}
                          aria-describedby={hoveredDatum?.key === event.id ? "timeline-event-tooltip" : undefined}
                          onMouseEnter={() => setHoveredDatum(tooltipState)}
                          onMouseLeave={() => hideTooltip(event.id)}
                          onFocus={() => setHoveredDatum(tooltipState)}
                          onBlur={() => hideTooltip(event.id)}
                        >
                          <rect
                            class="event-marker__hit"
                            x={markerX - EVENT_MARKER_HIT_RADIUS}
                            y={eventY - EVENT_MARKER_HIT_RADIUS}
                            width={EVENT_MARKER_HIT_RADIUS * 2}
                            height={EVENT_MARKER_HIT_RADIUS * 2}
                          />
                          <line
                            class="event-marker__anchor"
                            x1={markerX}
                            x2={markerX}
                            y1={eventScopeY}
                            y2={eventY - 5}
                          />
                          <rect
                            class="event-marker__focus"
                            x={markerX - 8}
                            y={eventY - 8}
                            width="16"
                            height="16"
                            rx="3"
                          />
                          <EventGlyph kind="custom_event" x={markerX - 5} y={eventY - 5} size={10} />
                        </g>
                      );
                    })}
                  </g>
                );
              })}
            </g>
          );
        })}

        <line
          class="playhead"
          x1={cursorX}
          x2={cursorX}
          y1="18"
          y2={height}
          data-mode={mode}
        />
        <rect class="playhead-label" x={Math.max(4, Math.min(width - 52, cursorX - 24))} y="1" width="48" height="18" rx="2" />
        <text class="playhead-text" x={Math.max(28, Math.min(width - 28, cursorX))} y="14" textAnchor="middle">
          {mode === "live" ? "NOW" : formatElapsed(cursor)}
        </text>
      </svg>
      {hoveredDatum !== null && tooltipPosition !== null ? (
        <div
          id="timeline-event-tooltip"
          class="event-tooltip"
          role="tooltip"
          data-kind={hoveredDatum.kind}
          style={{
            left: `${tooltipPosition.left}px`,
            top: `${tooltipPosition.top}px`,
          }}
        >
          <header>
            <span class="event-tooltip__kind">
              <EventGlyph kind={hoveredDatum.kind} size={15} />
              {eventKindLabel(hoveredDatum.kind)}
            </span>
            <time>{formatElapsed(hoveredDatum.at)}</time>
          </header>
          <strong>{hoveredDatum.label}</strong>
          <dl>
            {tooltipDetails.map(([term, value]) => (
              <div key={term}><dt>{term}</dt><dd>{value}</dd></div>
            ))}
          </dl>
        </div>
      ) : null}
    </div>
  );
}

function ActorLabels({
  mode,
  cursor,
  layout,
  selectedActorId,
  onSelectActor,
}: {
  readonly mode: TimelineMode;
  readonly cursor: number;
  readonly layout: TimelineLayout;
  readonly selectedActorId: string | null;
  readonly onSelectActor: (actorId: string) => void;
}): JSX.Element {
  return (
    <div class="actor-label-axis" style={{ height: `${layout.height}px` }}>
      <div class="scene-axis-label">
        <span>Scenes</span>
        <small>Run elapsed</small>
      </div>
      {layout.rows.map((row) => {
        const actor = row.actor;
        if (actor === null) {
          return (
            <div
              class="actor-slot-empty"
              key={`empty-${row.slot}`}
              style={{ top: `${row.top}px`, height: `${row.height}px` }}
              aria-hidden="true"
            />
          );
        }
        const state = actorState(actor, cursor);
        return (
          <button
            class="actor-label"
            type="button"
            key={actor.id}
            style={{ top: `${row.top}px`, height: `${row.height}px` }}
            data-cue-lanes={row.cueLaneCount}
            data-actor-id={actor.id}
            data-row-top={row.top}
            data-row-height={row.height}
            data-selected={actor.id === selectedActorId}
            data-state={state}
            onClick={() => onSelectActor(actor.id)}
          >
            <span class="actor-label__status" aria-hidden="true" />
            <span class="actor-label__copy">
              <strong>{actor.name}</strong>
              <small>{actor.role}</small>
            </span>
            {row.pinned ? <Pin class="actor-label__pin" aria-label="Pinned" /> : null}
            <span class="actor-label__state">{stateLabel(state)}</span>
            {mode === "live" ? <span class="actor-label__slot">L{row.slot + 1}</span> : null}
          </button>
        );
      })}
    </div>
  );
}

function TimelineSurface(props: {
  readonly data: TimelineData;
  readonly mode: TimelineMode;
  readonly range: TimelineRange;
  readonly cursor: number;
  readonly rows: readonly DisplayRow[];
  readonly selectedActorId: string | null;
  readonly followLive: boolean;
  readonly onLeaveLiveEdge: () => void;
  readonly onSelectActor: (actorId: string) => void;
}): JSX.Element {
  const scrollHost = useRef<HTMLDivElement | null>(null);
  const layout = useMemo(() => buildTimelineLayout(
    props.data,
    props.mode,
    props.range,
    props.cursor,
    props.rows,
  ), [props.cursor, props.data, props.mode, props.range, props.rows]);
  useLayoutEffect(() => {
    const element = scrollHost.current;
    if (element === null || props.mode !== "live" || !props.followLive) {
      return;
    }
    const scrollToLiveEdge = (): void => {
      element.scrollLeft = element.scrollWidth - element.clientWidth;
    };
    scrollToLiveEdge();
    if (typeof ResizeObserver === "undefined") {
      return;
    }
    const observer = new ResizeObserver(scrollToLiveEdge);
    observer.observe(element);
    return () => observer.disconnect();
  }, [props.followLive, props.mode, props.rows.length]);

  return (
    <div
      class="timeline-scroll"
      ref={scrollHost}
      aria-label="Actor-centered timeline"
      onScroll={(event) => {
        if (props.mode !== "live" || !props.followLive) {
          return;
        }
        const element = event.currentTarget;
        const distanceFromLiveEdge = element.scrollWidth - element.clientWidth - element.scrollLeft;
        if (distanceFromLiveEdge > 24) {
          props.onLeaveLiveEdge();
        }
      }}
    >
      <div class="timeline-grid">
        <ActorLabels
          mode={props.mode}
          cursor={props.cursor}
          layout={layout}
          selectedActorId={props.selectedActorId}
          onSelectActor={props.onSelectActor}
        />
        <TimelinePlot
          data={props.data}
          mode={props.mode}
          range={props.range}
          cursor={props.cursor}
          layout={layout}
          selectedActorId={props.selectedActorId}
          onSelectActor={props.onSelectActor}
        />
      </div>
    </div>
  );
}

function RunOverview({
  data,
  range,
  cursor,
  onSelectScene,
}: {
  readonly data: TimelineData;
  readonly range: TimelineRange;
  readonly cursor: number;
  readonly onSelectScene: (scene: SceneRecord) => void;
}): JSX.Element {
  return (
    <div class="run-overview" aria-label="Run history overview">
      <div class="run-overview__track">
        {data.scenes.map((scene) => {
          const left = (scene.start / data.totalTime) * 100;
          const end = scene.end ?? data.totalTime;
          const width = ((end - scene.start) / data.totalTime) * 100;
          return (
            <button
              key={scene.id}
              type="button"
              title={`Select ${scene.label}`}
              aria-label={`Select ${scene.label}`}
              class="run-overview__scene"
              style={{
                left: `${left}%`,
                width: `${width}%`,
                backgroundColor: SCENE_COLORS[scene.tone],
              }}
              onClick={() => onSelectScene(scene)}
            >
              <span>{scene.label}</span>
            </button>
          );
        })}
        <div
          class="run-overview__selection"
          style={{
            left: `${(range.start / data.totalTime) * 100}%`,
            width: `${((range.end - range.start) / data.totalTime) * 100}%`,
          }}
        />
        <div
          class="run-overview__cursor"
          style={{ left: `${(cursor / data.totalTime) * 100}%` }}
        />
      </div>
    </div>
  );
}

function Inspector({
  data,
  actor,
  cursor,
  mode,
  pinned,
  onClear,
}: {
  readonly data: TimelineData;
  readonly actor: ActorRecord | null;
  readonly cursor: number;
  readonly mode: TimelineMode;
  readonly pinned: boolean;
  readonly onClear: () => void;
}): JSX.Element {
  if (actor === null) {
    return (
      <aside class="timeline-inspector" aria-label="Timeline selection">
        <div class="inspector-empty">No timeline selection</div>
      </aside>
    );
  }
  const state = actorState(actor, cursor);
  const actorCues = data.cues.filter((cue) => cue.actorId === actor.id);
  const sceneNames = [...new Set(actorCues.map((cue) => sceneForId(data, cue.sceneId).label))];
  const visibleEnd = actor.end === null || cursor < actor.end ? cursor : actor.end;
  const activeCue = actorCues.find((cue) => cueState(cue, cursor) === "active") ?? null;
  return (
    <aside class="timeline-inspector" aria-label="Timeline selection">
      <header class="inspector-header">
        <div>
          <span class="inspector-kicker">Actor lifecycle</span>
          <h2>{actor.name}</h2>
        </div>
        <IconButton label="Clear timeline selection" onClick={onClear}>
          <X aria-hidden="true" />
        </IconButton>
      </header>
      <div class="inspector-status" data-state={state}>
        <span aria-hidden="true" />
        {stateLabel(state)}
        {pinned ? <Pin aria-label="Pinned outside Live window" /> : null}
      </div>
      <dl class="inspector-facts">
        <div><dt>Actor ID</dt><dd>{actor.id}</dd></div>
        <div><dt>Created</dt><dd>{formatElapsed(actor.start)}</dd></div>
        <div><dt>Destroyed</dt><dd>{actor.end === null ? "Open" : formatElapsed(actor.end)}</dd></div>
        <div><dt>Observed</dt><dd>{formatElapsed(Math.max(0, visibleEnd - actor.start))}</dd></div>
        <div><dt>Scenes</dt><dd>{sceneNames.length}</dd></div>
        <div><dt>Cues</dt><dd>{actorCues.length}</dd></div>
      </dl>
      <section class="inspector-activity" aria-label="Current Actor activity">
        <h3>{mode === "live" ? "Current activity" : "At playhead"}</h3>
        {activeCue === null ? (
          <p>{state === "future" ? "Not created at this time." : "No Cue is executing."}</p>
        ) : (
          <div>
            <strong>{activeCue.label}</strong>
            <span>{sceneForId(data, activeCue.sceneId).label}</span>
            <small>{formatElapsed(activeCue.execution)} to {activeCue.end === null ? "open" : formatElapsed(activeCue.end)}</small>
          </div>
        )}
      </section>
      <section class="inspector-scenes" aria-label="Actor Scene participation">
        <h3>Scene participation</h3>
        <ol>
          {sceneNames.map((name) => <li key={name}>{name}</li>)}
        </ol>
      </section>
    </aside>
  );
}

export interface ActorTimelineProps {
  readonly data: TimelineData;
  readonly historyData?: TimelineData | null;
  readonly historyStatus?: "idle" | "loading" | "ready" | "error";
  readonly historyError?: string | null;
  readonly livePaused: boolean;
  readonly unseenCount: bigint;
  readonly onPauseToggle: () => void;
  readonly onModeChange?: (mode: TimelineMode) => void;
}

export function ActorTimeline({
  data,
  historyData = null,
  historyStatus = "ready",
  historyError = null,
  livePaused,
  unseenCount,
  onPauseToggle,
  onModeChange,
}: ActorTimelineProps): JSX.Element {
  const [mode, setMode] = useState<TimelineMode>("live");
  const timelineData = mode === "history" && historyData !== null ? historyData : data;
  const [followLive, setFollowLive] = useState(true);
  const [liveWindow, setLiveWindow] = useState(DEFAULT_LIVE_WINDOW_SECONDS);
  const [historyRange, setHistoryRange] = useState<TimelineRange>(() => ({
    start: Math.max(0, data.totalTime - 120),
    end: data.totalTime,
  }));
  const [historyCursor, setHistoryCursor] = useState(() => Math.max(0, data.totalTime - 120));
  const [historyPlaying, setHistoryPlaying] = useState(false);
  const [historySpeed, setHistorySpeed] = useState(1);
  const [selectedActorId, setSelectedActorId] = useState<string | null>(
    data.actors.find((actor) => liveActorVisible(actor, data.liveNow, 60, null))?.id ?? null,
  );

  useEffect(() => {
    setHistoryRange((current) => {
      const end = Math.min(timelineData.totalTime, current.end);
      const start = Math.min(current.start, end);
      return start === current.start && end === current.end ? current : { start, end };
    });
    setHistoryCursor((current) => Math.min(current, timelineData.totalTime));
  }, [timelineData.totalTime]);

  useEffect(() => {
    if (mode !== "live" || selectedActorId === null) {
      return;
    }
    const selected = data.actors.find((actor) => actor.id === selectedActorId);
    if (
      selected === undefined
      || !liveActorVisible(selected, data.liveNow, liveWindow, null)
    ) {
      setSelectedActorId(null);
    }
  }, [data.actors, data.liveNow, liveWindow, mode, selectedActorId]);

  useEffect(() => {
    if (mode !== "history" || !historyPlaying) {
      return;
    }
    const interval = window.setInterval(() => {
      setHistoryCursor((current) => {
        const next = Math.min(historyRange.end, current + historySpeed * 0.25);
        if (next >= historyRange.end) {
          setHistoryPlaying(false);
        }
        return next;
      });
    }, 250);
    return () => window.clearInterval(interval);
  }, [historyPlaying, historyRange.end, historySpeed, mode]);

  const range = mode === "live"
    ? liveTimelineRange(timelineData.liveNow, liveWindow)
    : historyRange;
  const cursor = mode === "live" ? timelineData.liveNow : historyCursor;
  const rows = useMemo(
    () => actorRows(timelineData, mode, range, cursor, liveWindow, selectedActorId),
    [cursor, timelineData, liveWindow, mode, range.end, range.start, selectedActorId],
  );
  const selectedActor = timelineData.actors.find((actor) => actor.id === selectedActorId) ?? null;
  const selectedRow = rows.find((row) => row.actor?.id === selectedActorId) ?? null;
  const activeActors = timelineData.actors.filter((actor) => actorState(actor, cursor) === "active").length;
  const visibleActors = rows.filter((row) => row.actor !== null).length;
  const minimumHistoryRange = Math.min(HISTORY_MIN_RANGE, Math.max(0.25, timelineData.totalTime));
  const historyReady = onModeChange === undefined
    || (historyStatus === "ready" && historyData !== null);

  const changeMode = (next: TimelineMode): void => {
    setMode(next);
    onModeChange?.(next);
    setHistoryPlaying(false);
    if (next === "live") {
      setFollowLive(true);
      return;
    }
    const start = Math.max(0, data.totalTime - 120);
    setHistoryRange({ start, end: data.totalTime });
    setHistoryCursor(start);
  };
  const changeHistoryStart = (value: number): void => {
    const next = Math.max(0, Math.min(value, historyRange.end - minimumHistoryRange));
    setHistoryRange({ start: next, end: historyRange.end });
    setHistoryCursor((current) => Math.max(next, current));
  };
  const changeHistoryEnd = (value: number): void => {
    const next = Math.min(
      timelineData.totalTime,
      Math.max(value, historyRange.start + minimumHistoryRange),
    );
    setHistoryRange({ start: historyRange.start, end: next });
    setHistoryCursor((current) => Math.min(next, current));
  };
  const selectSceneRange = (scene: SceneRecord): void => {
    const start = Math.max(0, scene.start - 4);
    const end = Math.min(
      timelineData.totalTime,
      (scene.end ?? timelineData.totalTime) + 4,
    );
    setHistoryRange({ start, end });
    setHistoryCursor(scene.start);
    setHistoryPlaying(false);
  };

  return (
    <div class="timeline-app" data-mode={mode}>
      <header class="app-header">
        <div class="brand-block">
          <Activity aria-hidden="true" />
          <div>
            <h1>Troupe Timeline</h1>
            <span>{timelineData.productionName}</span>
          </div>
        </div>
        <dl class="run-summary">
          <div>
            <dt>Connection</dt>
            <dd><Archive aria-hidden="true" /> {timelineData.connectionLabel}</dd>
          </div>
          <div>
            <dt>Mode</dt>
            <dd data-emphasis="true">
              {mode === "live"
                ? livePaused ? "Live paused" : followLive ? "Following live" : "Live detached"
                : historyStatus === "loading" ? "Loading history"
                  : historyStatus === "error" ? "History unavailable"
                    : historyPlaying ? "Replaying history" : "Frozen history"}
            </dd>
          </div>
          <div>
            <dt>Elapsed</dt>
            <dd><Clock3 aria-hidden="true" /> {formatElapsed(cursor)}</dd>
          </div>
          <div>
            <dt>Actors</dt>
            <dd>{activeActors} active / {visibleActors} visible</dd>
          </div>
          <div>
            <dt>Watermark</dt>
            <dd title={`Run ${timelineData.outcomeLabel}`}>{timelineData.watermark}</dd>
          </div>
        </dl>
      </header>

      <section class="control-strip" aria-label="Timeline controls">
        <ModeSwitch mode={mode} onChange={changeMode} />
        {mode === "live" ? (
          <div class="mode-controls">
            <IconButton
              label={livePaused ? "Resume live diagnostics" : "Pause live diagnostics"}
              onClick={onPauseToggle}
              pressed={livePaused}
            >
              {livePaused ? <Play aria-hidden="true" /> : <Pause aria-hidden="true" />}
            </IconButton>
            <IconButton
              label="Follow live edge"
              onClick={() => setFollowLive(true)}
              pressed={followLive}
            >
              <Radio aria-hidden="true" />
            </IconButton>
            <label class="select-control">
              <span>Window</span>
              <select value={liveWindow} onChange={(event) => setLiveWindow(Number(event.currentTarget.value))}>
                <option value="10">10 sec</option>
                <option value="30">30 sec</option>
                <option value="60">60 sec</option>
                <option value="120">120 sec</option>
              </select>
            </label>
            {unseenCount > 0n ? <span class="demo-rate">{unseenCount.toString()} unseen</span> : null}
          </div>
        ) : (
          <div class="mode-controls history-playback">
            <IconButton
              label={historyPlaying ? "Pause History playback" : "Play History range"}
              onClick={() => {
                if (historyCursor >= historyRange.end) {
                  setHistoryCursor(historyRange.start);
                }
                setHistoryPlaying((playing) => !playing);
              }}
              pressed={historyPlaying}
              disabled={!historyReady}
            >
              {historyPlaying ? <Pause aria-hidden="true" /> : <Play aria-hidden="true" />}
            </IconButton>
            <IconButton
              label="Restart History range"
              onClick={() => {
                setHistoryCursor(historyRange.start);
                setHistoryPlaying(false);
              }}
              disabled={!historyReady}
            >
              <RotateCcw aria-hidden="true" />
            </IconButton>
            <div class="segmented segmented--compact" role="group" aria-label="Playback speed">
              {[0.5, 1, 2, 4].map((speed) => (
                <button
                  type="button"
                  key={speed}
                  data-active={historySpeed === speed}
                  aria-pressed={historySpeed === speed}
                  disabled={!historyReady}
                  onClick={() => setHistorySpeed(speed)}
                >
                  {speed}x
                </button>
              ))}
            </div>
          </div>
        )}
        <output class="range-readout" aria-label="Visible timeline range">
          {formatElapsed(range.start)} - {formatElapsed(range.end)}
        </output>
        {mode === "history" && historyStatus === "loading" ? (
          <span class="history-capture-status" role="status">Loading frozen history</span>
        ) : null}
        {mode === "history" && historyStatus === "error" ? (
          <span class="history-capture-status" role="alert" title={historyError ?? undefined}>
            History unavailable
          </span>
        ) : null}
      </section>

      {mode === "history" ? (
        <section class="history-controls" aria-label="History range selection">
          <RunOverview
            data={timelineData}
            range={historyRange}
            cursor={historyCursor}
            onSelectScene={selectSceneRange}
          />
          <div class="range-sliders">
            <label>
              <span>Range start <output>{formatElapsed(historyRange.start)}</output></span>
              <input
                type="range"
                aria-label="History range start"
                min="0"
                max={Math.max(0, timelineData.totalTime - minimumHistoryRange)}
                step="0.1"
                value={historyRange.start}
                disabled={!historyReady}
                onInput={(event) => changeHistoryStart(Number(event.currentTarget.value))}
              />
            </label>
            <label>
              <span>Range end <output>{formatElapsed(historyRange.end)}</output></span>
              <input
                type="range"
                aria-label="History range end"
                min={minimumHistoryRange}
                max={timelineData.totalTime}
                step="0.1"
                value={historyRange.end}
                disabled={!historyReady}
                onInput={(event) => changeHistoryEnd(Number(event.currentTarget.value))}
              />
            </label>
            <label>
              <span>Playhead <output>{formatElapsed(historyCursor)}</output></span>
              <input
                type="range"
                aria-label="History playhead"
                min={historyRange.start}
                max={historyRange.end}
                step="0.25"
                value={historyCursor}
                disabled={!historyReady}
                onInput={(event) => {
                  setHistoryCursor(Number(event.currentTarget.value));
                  setHistoryPlaying(false);
                }}
              />
            </label>
          </div>
        </section>
      ) : null}

      <div class="workspace">
        <main class="timeline-pane">
          <TimelineSurface
            data={timelineData}
            mode={mode}
            range={range}
            cursor={cursor}
            rows={rows}
            selectedActorId={selectedActorId}
            followLive={followLive}
            onLeaveLiveEdge={() => setFollowLive(false)}
            onSelectActor={setSelectedActorId}
          />
        </main>
        <Inspector
          data={timelineData}
          actor={selectedActor}
          cursor={cursor}
          mode={mode}
          pinned={selectedRow?.pinned ?? false}
          onClear={() => setSelectedActorId(null)}
        />
      </div>
    </div>
  );
}
