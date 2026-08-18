import type {
  DiagnosticEvent,
  DiagnosticScope,
  SpanStartedEvent,
} from "../protocol/event.ts";
import type { U64String } from "../protocol/decimal.ts";
import type {
  DiagnosticState,
  ProjectedSpan,
} from "../state/model.ts";
import { presentedLiveEdge } from "../state/reducer.ts";
import type { LiveDiagnosticsState } from "../live/reconnect.ts";
import type {
  ActorRecord,
  ActRecord,
  CueRecord,
  CustomEventRecord,
  CustomSpanRecord,
  DiagnosticAttribute,
  SceneRecord,
  SystemEventRecord,
  TimelineData,
} from "./actor_timeline_model.ts";


type BuiltInSpan = ProjectedSpan & { readonly start: SpanStartedEvent };

const SCENE_TONES: readonly SceneRecord["tone"][] = ["green", "blue", "amber", "violet"];

function isBuiltIn(span: ProjectedSpan): span is BuiltInSpan {
  return span.start?.kind === "span_started";
}

function elapsedSeconds(value: U64String): number {
  return Number(BigInt(value)) / 1_000_000_000;
}

function detailRecord(value: unknown): Readonly<Record<string, unknown>> {
  return typeof value === "object" && value !== null
    ? value as Readonly<Record<string, unknown>>
    : {};
}

function stringDetail(value: unknown, key: string): string | null {
  const candidate = detailRecord(value)[key];
  return typeof candidate === "string" && candidate.length > 0 ? candidate : null;
}

function taggedValue(value: unknown): string | number | boolean | readonly (string | number | boolean)[] {
  if (typeof value !== "object" || value === null) {
    return String(value);
  }
  const tagged = value as { readonly type?: string; readonly value?: unknown };
  switch (tagged.type) {
    case "boolean":
      return typeof tagged.value === "boolean" ? tagged.value : String(tagged.value);
    case "integer":
      try {
        return Number(BigInt(String(tagged.value)));
      } catch {
        return String(tagged.value);
      }
    case "decimal":
      return Number(tagged.value);
    case "string":
      return typeof tagged.value === "string" ? tagged.value : String(tagged.value);
    case "list":
      return Array.isArray(tagged.value)
        ? tagged.value.map((item) => taggedValue(item)).map((item) => (
          typeof item === "string" || typeof item === "number" || typeof item === "boolean"
            ? item
            : String(item)
        ))
        : [];
    case "null":
      return "null";
    default:
      return String(tagged.value ?? "");
  }
}

function attributes(value: unknown): Readonly<Record<string, DiagnosticAttribute>> {
  if (typeof value !== "object" || value === null) {
    return {};
  }
  const result: Record<string, DiagnosticAttribute> = {};
  for (const [key, item] of Object.entries(value)) {
    result[key] = taggedValue(item);
  }
  return result;
}

function compareStart(left: ProjectedSpan, right: ProjectedSpan): number {
  const leftNs = BigInt(left.start?.elapsed_ns ?? left.finish?.elapsed_ns ?? "0");
  const rightNs = BigInt(right.start?.elapsed_ns ?? right.finish?.elapsed_ns ?? "0");
  return leftNs < rightNs ? -1 : leftNs > rightNs ? 1 : 0;
}

function projectTimelineSpans(events: readonly DiagnosticEvent[]): readonly ProjectedSpan[] {
  const byId = new Map<U64String, ProjectedSpan>();
  for (const event of events) {
    if (event.kind === "span_started" || event.kind === "custom_span_started") {
      const existing = byId.get(event.sequence);
      byId.set(event.sequence, {
        span_id: event.sequence,
        start: event,
        finish: existing?.finish ?? null,
      });
      continue;
    }
    if (event.kind !== "span_finished" && event.kind !== "custom_span_finished") {
      continue;
    }
    const existing = byId.get(event.span_id);
    byId.set(event.span_id, {
      span_id: event.span_id,
      start: existing?.start ?? null,
      finish: event,
    });
  }
  return [...byId.values()].sort(compareStart);
}

function unionEvents(state: DiagnosticState): readonly DiagnosticEvent[] {
  const edge = presentedLiveEdge(state);
  const bySequence = new Map<string, DiagnosticEvent>();
  for (const event of state.windows.visible?.events ?? []) {
    bySequence.set(event.sequence, event);
  }
  for (const event of edge.events) {
    bySequence.set(event.sequence, event);
  }
  return [...bySequence.values()].sort((left, right) => {
    const a = BigInt(left.sequence);
    const b = BigInt(right.sequence);
    return a < b ? -1 : a > b ? 1 : 0;
  });
}

function scopeId(scope: DiagnosticScope, field: "scene_id" | "actor_id" | "cue_id" | "act_id"): string | null {
  return scope[field];
}

function buildScenes(spans: readonly BuiltInSpan[]): readonly SceneRecord[] {
  const byId = new Map<string, SceneRecord>();
  for (const span of spans) {
    if (span.start.span_kind !== "scene.lifecycle") {
      continue;
    }
    const id = scopeId(span.start.scope, "scene_id");
    if (id === null) {
      continue;
    }
    const start = elapsedSeconds(span.start.elapsed_ns);
    const end = span.finish === null ? null : elapsedSeconds(span.finish.elapsed_ns);
    const previous = byId.get(id);
    if (previous === undefined || start < previous.start) {
      byId.set(id, {
        id,
        label: `Scene ${id}`,
        start,
        end,
        outcome: span.finish?.outcome ?? null,
        tone: SCENE_TONES[byId.size % SCENE_TONES.length]!,
      });
    } else if (end !== null && (previous.end === null || end > previous.end)) {
      byId.set(id, { ...previous, end, outcome: span.finish?.outcome ?? previous.outcome });
    }
  }
  return [...byId.values()]
    .sort((left, right) => left.start - right.start)
    .map((scene, index) => ({ ...scene, tone: SCENE_TONES[index % SCENE_TONES.length]! }));
}

function addFallbackScenes(
  scenes: readonly SceneRecord[],
  cues: readonly CueRecord[],
  events: readonly DiagnosticEvent[],
): readonly SceneRecord[] {
  const byId = new Map(scenes.map((scene) => [scene.id, scene]));
  const starts = new Map<string, number>();
  const observe = (id: string | null, at: number): void => {
    if (id === null) {
      return;
    }
    const previous = starts.get(id);
    starts.set(id, previous === undefined ? at : Math.min(previous, at));
  };
  for (const cue of cues) {
    observe(cue.sceneId, cue.admitted);
  }
  for (const event of events) {
    observe(scopeId(event.scope, "scene_id"), elapsedSeconds(event.elapsed_ns));
  }
  for (const [id, start] of starts) {
    if (!byId.has(id)) {
      byId.set(id, {
        id,
        label: `Scene ${id}`,
        start,
        end: null,
        outcome: null,
        tone: SCENE_TONES[byId.size % SCENE_TONES.length]!,
      });
    }
  }
  return [...byId.values()]
    .sort((left, right) => left.start - right.start || left.id.localeCompare(right.id))
    .map((scene, index) => ({ ...scene, tone: SCENE_TONES[index % SCENE_TONES.length]! }));
}

function buildActors(
  spans: readonly ProjectedSpan[],
  events: readonly DiagnosticEvent[],
): readonly ActorRecord[] {
  const byId = new Map<string, ActorRecord>();
  const actorSpans = spans.filter((span): span is BuiltInSpan => (
    isBuiltIn(span) && span.start.span_kind === "actor.handle_lifetime"
  ));
  for (const span of actorSpans) {
    const id = scopeId(span.start.scope, "actor_id");
    if (id === null) {
      continue;
    }
    const candidate: ActorRecord = {
      id,
      name: stringDetail(span.start.detail, "display_name") ?? id,
      role: stringDetail(span.start.detail, "actor_type") ?? "Actor",
      start: elapsedSeconds(span.start.elapsed_ns),
      end: span.finish === null ? null : elapsedSeconds(span.finish.elapsed_ns),
      outcome: span.finish?.outcome ?? null,
      liveSlot: byId.size,
    };
    const previous = byId.get(id);
    if (previous === undefined || candidate.start < previous.start) {
      byId.set(id, candidate);
    } else if (candidate.end !== null && (previous.end === null || candidate.end > previous.end)) {
      byId.set(id, {
        ...previous,
        end: candidate.end,
        outcome: candidate.outcome ?? previous.outcome,
      });
    }
  }

  // A bounded or partially recovered snapshot can omit the lifetime span while
  // retaining actor-scoped work. Keep that Actor visible with an open fallback
  // lifetime; a real lifetime span always wins when it is present.
  for (const event of events) {
    const id = scopeId(event.scope, "actor_id");
    if (id === null) {
      continue;
    }
    const cast = event.kind === "instant_occurred" && event.instant_kind === "actor.cast"
      ? event
      : null;
    const previous = byId.get(id);
    if (previous !== undefined) {
      if (cast !== null && previous.name === id) {
        byId.set(id, {
          ...previous,
          name: stringDetail(cast.detail, "display_name") ?? previous.name,
          role: stringDetail(cast.detail, "actor_type") ?? previous.role,
        });
      }
      continue;
    }
    byId.set(id, {
      id,
      name: cast === null ? id : stringDetail(cast.detail, "display_name") ?? id,
      role: cast === null ? "Actor" : stringDetail(cast.detail, "actor_type") ?? "Actor",
      start: elapsedSeconds(event.elapsed_ns),
      end: null,
      outcome: null,
      liveSlot: byId.size,
    });
  }
  for (const span of spans) {
    if (span.start === null) {
      continue;
    }
    const id = scopeId(span.start.scope, "actor_id");
    if (id === null || byId.has(id)) {
      continue;
    }
    byId.set(id, {
      id,
      name: id,
      role: "Actor",
      start: elapsedSeconds(span.start.elapsed_ns),
      end: null,
      outcome: null,
      liveSlot: byId.size,
    });
  }
  return [...byId.values()]
    .sort((left, right) => left.start - right.start || left.id.localeCompare(right.id))
    .map((actor, index) => ({ ...actor, liveSlot: index % 6 }));
}

interface CueParts {
  readonly id: string;
  readonly sceneId: string;
  readonly actorId: string;
  admitted: number | null;
  execution: number | null;
  end: number | null;
  outcome: CueRecord["outcome"];
}

function buildCues(
  spans: readonly BuiltInSpan[],
  events: readonly DiagnosticEvent[],
  liveNow: number,
): readonly CueRecord[] {
  const byId = new Map<string, CueParts>();
  const ensure = (scope: DiagnosticScope): CueParts | null => {
    const id = scopeId(scope, "cue_id");
    const sceneId = scopeId(scope, "scene_id");
    const actorId = scopeId(scope, "actor_id");
    if (id === null || sceneId === null || actorId === null) {
      return null;
    }
    const existing = byId.get(id);
    if (existing !== undefined) {
      return existing;
    }
    const created: CueParts = {
      id,
      sceneId,
      actorId,
      admitted: null,
      execution: null,
      end: null,
      outcome: null,
    };
    byId.set(id, created);
    return created;
  };
  for (const span of spans) {
    const cue = ensure(span.start.scope);
    if (cue === null) {
      continue;
    }
    if (span.start.span_kind === "cue.mailbox_wait") {
      cue.admitted = cue.admitted === null
        ? elapsedSeconds(span.start.elapsed_ns)
        : Math.min(cue.admitted, elapsedSeconds(span.start.elapsed_ns));
      if (span.finish !== null && cue.execution === null) {
        cue.execution = elapsedSeconds(span.finish.elapsed_ns);
      }
    } else if (span.start.span_kind === "cue.execution") {
      cue.execution = elapsedSeconds(span.start.elapsed_ns);
      cue.end = span.finish === null ? null : elapsedSeconds(span.finish.elapsed_ns);
      cue.outcome = span.finish?.outcome ?? null;
    }
  }
  for (const event of events) {
    const cue = ensure(event.scope);
    if (cue === null || event.kind !== "instant_occurred") {
      continue;
    }
    if (["cue.admitted", "cue.enqueued", "cue.dispatched"].includes(event.instant_kind)) {
      const at = elapsedSeconds(event.elapsed_ns);
      cue.admitted = cue.admitted === null ? at : Math.min(cue.admitted, at);
    }
  }
  return [...byId.values()].map((cue) => {
    const admitted = cue.admitted ?? cue.execution ?? liveNow;
    const execution = cue.execution ?? (cue.end === null ? liveNow + 0.001 : admitted);
    return {
      id: cue.id,
      label: `Cue ${cue.id}`,
      sceneId: cue.sceneId,
      actorId: cue.actorId,
      admitted,
      execution,
      end: cue.end,
      outcome: cue.outcome,
      events: [],
    };
  }).sort((left, right) => left.admitted - right.admitted);
}

function buildActs(spans: readonly BuiltInSpan[]): readonly ActRecord[] {
  return spans
    .filter((span) => span.start.span_kind === "act.lifecycle")
    .flatMap((span): ActRecord[] => {
      const cueId = scopeId(span.start.scope, "cue_id");
      const id = scopeId(span.start.scope, "act_id");
      if (cueId === null || id === null) {
        return [];
      }
      const model = stringDetail(span.start.detail, "effective_model");
      return [{
        id,
        label: model === null ? `Act ${id}` : `Act ${id} · ${model}`,
        cueId,
        start: elapsedSeconds(span.start.elapsed_ns),
        end: span.finish === null ? null : elapsedSeconds(span.finish.elapsed_ns),
        outcome: span.finish?.outcome ?? null,
      }];
    })
    .sort((left, right) => left.start - right.start);
}

function buildCustomSpans(spans: readonly ProjectedSpan[]): readonly CustomSpanRecord[] {
  return spans.flatMap((span): CustomSpanRecord[] => {
    if (span.start?.kind !== "custom_span_started") {
      return [];
    }
    const cueId = scopeId(span.start.scope, "cue_id");
    if (cueId === null) {
      return [];
    }
    return [{
      id: span.span_id,
      name: span.start.name,
      cueId,
      actId: scopeId(span.start.scope, "act_id"),
      parentSpanId: span.start.parent_span_id,
      start: elapsedSeconds(span.start.elapsed_ns),
      end: span.finish === null ? null : elapsedSeconds(span.finish.elapsed_ns),
      outcome: span.finish?.outcome ?? null,
      attributes: attributes(span.start.attributes),
    }];
  }).sort((left, right) => left.start - right.start);
}

function buildCustomEvents(events: readonly DiagnosticEvent[]): readonly CustomEventRecord[] {
  return events.flatMap((event): CustomEventRecord[] => {
    if (event.kind !== "custom_instant_occurred") {
      return [];
    }
    const cueId = scopeId(event.scope, "cue_id");
    if (cueId === null) {
      return [];
    }
    return [{
      id: event.sequence,
      name: event.name,
      cueId,
      actId: scopeId(event.scope, "act_id"),
      containingSpanId: event.containing_span_id,
      at: elapsedSeconds(event.elapsed_ns),
      severity: event.severity,
      attributes: attributes(event.attributes),
    }];
  });
}

function buildSystemEvents(
  state: DiagnosticState,
  cues: readonly CueRecord[],
): ReadonlyMap<string, readonly SystemEventRecord[]> {
  const edge = presentedLiveEdge(state);
  const byCue = new Map<string, SystemEventRecord[]>();
  const append = (cueId: string | null, event: SystemEventRecord): void => {
    if (cueId === null) {
      return;
    }
    const list = byCue.get(cueId) ?? [];
    list.push(event);
    byCue.set(cueId, list);
  };
  const latestTools = new Map<string, typeof edge.projection.tools.items[number]>();
  for (const fact of edge.projection.tools.items) {
    const key = fact.tool_call_id ?? fact.span_id ?? fact.sequence;
    const previous = latestTools.get(key);
    if (previous === undefined || BigInt(previous.sequence) < BigInt(fact.sequence)) {
      latestTools.set(key, fact);
    }
  }
  for (const fact of latestTools.values()) {
    append(scopeId(fact.scope, "cue_id"), {
      id: `tool:${fact.tool_call_id ?? fact.sequence}`,
      at: elapsedSeconds(fact.elapsed_ns),
      kind: "tool",
      label: fact.title ?? "Tool call",
      actId: scopeId(fact.scope, "act_id") ?? "",
      outcome: fact.outcome,
    });
  }
  for (const message of edge.projection.messages.items) {
    const text = message.text.trim().replace(/\s+/g, " ");
    append(scopeId(message.scope, "cue_id"), {
      id: `message:${message.message_id}`,
      at: elapsedSeconds(message.latest_elapsed_ns),
      kind: "message",
      label: text.length > 0 ? text.slice(0, 42) : "Agent message",
      actId: scopeId(message.scope, "act_id") ?? "",
      outcome: message.completion === null ? null : "completed",
    });
  }
  for (const event of unionEvents(state)) {
    if (event.kind !== "instant_occurred" || !event.instant_kind.startsWith("result.")) {
      continue;
    }
    append(scopeId(event.scope, "cue_id"), {
      id: `result:${event.sequence}`,
      at: elapsedSeconds(event.elapsed_ns),
      kind: "message",
      label: event.instant_kind.replace("result.", "Result · "),
      actId: scopeId(event.scope, "act_id") ?? "",
      outcome: event.instant_kind === "result.accepted" ? "completed" : null,
    });
  }
  const cueIds = new Set(cues.map((cue) => cue.id));
  return new Map([...byCue.entries()].map(([id, items]) => [
    id,
    items.filter((item) => cueIds.has(id)).sort((left, right) => left.at - right.at),
  ]));
}

function buildCapturedSystemEvents(
  events: readonly DiagnosticEvent[],
  spans: readonly ProjectedSpan[],
  cues: readonly CueRecord[],
): ReadonlyMap<string, readonly SystemEventRecord[]> {
  const byCue = new Map<string, SystemEventRecord[]>();
  const append = (cueId: string | null, event: SystemEventRecord): void => {
    if (cueId === null) {
      return;
    }
    const items = byCue.get(cueId) ?? [];
    items.push(event);
    byCue.set(cueId, items);
  };

  const spanById = new Map(spans.map((span) => [span.span_id, span]));
  const tools = new Map<string, SystemEventRecord>();
  const toolCueIds = new Map<string, string | null>();
  const messages = new Map<string, {
    scope: DiagnosticScope;
    text: string;
    at: number;
    outcome: SystemEventRecord["outcome"];
  }>();

  for (const event of events) {
    if (event.kind === "span_started" && event.span_kind === "tool.call") {
      const detail = detailRecord(event.detail);
      const key = event.scope.tool_call_id ?? event.sequence;
      tools.set(key, {
        id: `tool:${key}`,
        at: elapsedSeconds(event.elapsed_ns),
        kind: "tool",
        label: typeof detail.title === "string" ? detail.title : "Tool call",
        actId: event.scope.act_id ?? "",
        outcome: spanById.get(event.sequence)?.finish?.outcome ?? null,
      });
      toolCueIds.set(key, scopeId(event.scope, "cue_id"));
      continue;
    }
    if (event.kind === "instant_occurred" && event.instant_kind === "tool.updated") {
      const key = event.scope.tool_call_id ?? event.containing_span_id ?? event.sequence;
      const previous = tools.get(key);
      const detail = detailRecord(event.detail);
      tools.set(key, {
        id: `tool:${key}`,
        at: elapsedSeconds(event.elapsed_ns),
        kind: "tool",
        label: typeof detail.title === "string" ? detail.title : previous?.label ?? "Tool call",
        actId: event.scope.act_id ?? previous?.actId ?? "",
        outcome: previous?.outcome ?? null,
      });
      if (!toolCueIds.has(key)) {
        toolCueIds.set(key, scopeId(event.scope, "cue_id"));
      }
      continue;
    }
    if (event.kind === "agent_message_delta") {
      const previous = messages.get(event.message_id);
      messages.set(event.message_id, {
        scope: previous?.scope ?? event.scope,
        text: `${previous?.text ?? ""}${event.text_delta}`.slice(0, 256),
        at: elapsedSeconds(event.elapsed_ns),
        outcome: null,
      });
      continue;
    }
    if (event.kind === "agent_message_completed") {
      const previous = messages.get(event.message_id);
      messages.set(event.message_id, {
        scope: previous?.scope ?? event.scope,
        text: previous?.text ?? "",
        at: elapsedSeconds(event.elapsed_ns),
        outcome: "completed",
      });
      continue;
    }
    if (event.kind === "instant_occurred" && event.instant_kind.startsWith("result.")) {
      append(scopeId(event.scope, "cue_id"), {
        id: `result:${event.sequence}`,
        at: elapsedSeconds(event.elapsed_ns),
        kind: "message",
        label: event.instant_kind.replace("result.", "Result · "),
        actId: scopeId(event.scope, "act_id") ?? "",
        outcome: event.instant_kind === "result.accepted" ? "completed" : null,
      });
    }
  }

  for (const [key, tool] of tools) {
    append(toolCueIds.get(key) ?? null, tool);
  }
  for (const [messageId, message] of messages) {
    const text = message.text.trim().replace(/\s+/g, " ");
    append(scopeId(message.scope, "cue_id"), {
      id: `message:${messageId}`,
      at: message.at,
      kind: "message",
      label: text.length > 0 ? text.slice(0, 42) : "Agent message",
      actId: scopeId(message.scope, "act_id") ?? "",
      outcome: message.outcome,
    });
  }

  const cueIds = new Set(cues.map((cue) => cue.id));
  return new Map([...byCue.entries()].map(([id, items]) => [
    id,
    items.filter(() => cueIds.has(id)).sort((left, right) => left.at - right.at),
  ]));
}

export interface ProductionTimelineMeta {
  readonly productionName?: string;
}

export interface CapturedTimelineMeta extends ProductionTimelineMeta {
  readonly connectionLabel: string;
  readonly outcomeLabel: string;
}

function assembleTimelineData(
  spans: readonly ProjectedSpan[],
  events: readonly DiagnosticEvent[],
  liveNow: number,
  watermark: string,
  meta: CapturedTimelineMeta,
  systemEvents: (
    cues: readonly CueRecord[],
  ) => ReadonlyMap<string, readonly SystemEventRecord[]>,
): TimelineData {
  const builtInSpans = spans.filter(isBuiltIn).sort(compareStart);
  const customSpans = buildCustomSpans(spans);
  const customEvents = buildCustomEvents(events);
  const actors = buildActors(spans, events);
  const cuesWithoutEvents = buildCues(builtInSpans, events, liveNow);
  const scenes = addFallbackScenes(buildScenes(builtInSpans), cuesWithoutEvents, events);
  const acts = buildActs(builtInSpans);
  const eventsByCue = systemEvents(cuesWithoutEvents);
  const cues = cuesWithoutEvents.map((cue) => ({
    ...cue,
    events: eventsByCue.get(cue.id) ?? [],
  }));
  const totalTime = Math.max(
    1,
    liveNow,
    ...scenes.flatMap((item) => [item.start, item.end ?? 0]),
    ...actors.flatMap((item) => [item.start, item.end ?? 0]),
    ...cues.flatMap((item) => [item.admitted, item.end ?? 0]),
    ...acts.flatMap((item) => [item.start, item.end ?? 0]),
    ...customSpans.flatMap((item) => [item.start, item.end ?? 0]),
    ...customEvents.map((item) => item.at),
  );
  return {
    scenes,
    actors,
    cues,
    acts,
    customSpans,
    customEvents,
    totalTime,
    liveNow,
    watermark,
    productionName: meta.productionName ?? "Production",
    connectionLabel: meta.connectionLabel,
    outcomeLabel: meta.outcomeLabel,
    liveSlotCount: 6,
  };
}

export function selectCapturedTimelineData(
  events: readonly DiagnosticEvent[],
  watermark: U64String,
  meta: CapturedTimelineMeta,
): TimelineData {
  const spans = projectTimelineSpans(events);
  const liveNow = events.reduce(
    (maximum, event) => Math.max(maximum, elapsedSeconds(event.elapsed_ns)),
    0,
  );
  return assembleTimelineData(
    spans,
    events,
    liveNow,
    watermark,
    meta,
    (cues) => buildCapturedSystemEvents(events, spans, cues),
  );
}

export function selectProductionTimelineData(
  state: DiagnosticState,
  live: Pick<LiveDiagnosticsState, "connection" | "outcome">,
  meta: ProductionTimelineMeta = {},
): TimelineData {
  const edge = presentedLiveEdge(state);
  const events = unionEvents(state);
  const liveNow = elapsedSeconds(edge.observed_elapsed_ns);
  return assembleTimelineData(
    edge.projection.spans.items,
    events,
    liveNow,
    state.cursor.committed_watermark,
    {
      ...meta,
      connectionLabel: live.connection === "archive" ? "Archive" : live.connection,
      outcomeLabel: live.outcome ?? "running",
    },
    (cues) => buildSystemEvents(state, cues),
  );
}
