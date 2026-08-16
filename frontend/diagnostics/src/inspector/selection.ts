import type { U64String } from "../protocol/decimal.ts";
import type {
  DiagnosticEvent,
  DiagnosticEventKind,
  DiagnosticScope,
} from "../protocol/event.ts";
import type {
  PresentationFilters,
  SelectionReference,
} from "../state/model.ts";


const SCOPE_FIELDS = [
  "scene_id",
  "actor_id",
  "cue_id",
  "effect_id",
  "act_id",
  "tool_call_id",
  "session_generation",
] as const;

type ScopeStringField = Exclude<typeof SCOPE_FIELDS[number], "session_generation">;

export type SelectionHighlight = "none" | "related" | "selected";
export type EventErrorFilter = "all" | "errors_only" | "errors_and_gaps";

export interface EventQueryState extends PresentationFilters {
  readonly event_kinds: readonly DiagnosticEventKind[];
  readonly error_filter: EventErrorFilter;
}

export const EMPTY_EVENT_QUERY: EventQueryState = {
  actor_id: null,
  error_filter: "all",
  event_kinds: [],
  scene_id: null,
  text: "",
};

export function hasActiveEventQuery(query: EventQueryState): boolean {
  return query.actor_id !== null
    || query.error_filter !== "all"
    || query.event_kinds.length > 0
    || query.scene_id !== null
    || query.text.length > 0;
}

export interface SelectionElapsedRange {
  readonly start_ns: U64String;
  readonly end_ns: U64String;
}

export interface ResolvedSelection {
  readonly reference: SelectionReference;
  readonly scope: DiagnosticScope;
  readonly elapsed_range: SelectionElapsedRange;
  readonly event_sequences: readonly U64String[];
}

export function eventSelectionReference(sequence: U64String): SelectionReference {
  return { kind: "event", id: `sequence:${sequence}` };
}

export function spanSelectionReference(spanId: U64String): SelectionReference {
  return { kind: "span", id: `span:${spanId}` };
}

export function messageSelectionReference(messageId: string): SelectionReference {
  return { kind: "message", id: messageId };
}

export function scopeFieldSelectionReference(
  field: ScopeStringField,
  id: string,
): SelectionReference {
  return { kind: "scope", id: `${field}:${id}` };
}

export function scopeSelectionReference(scope: DiagnosticScope): SelectionReference {
  const values = SCOPE_FIELDS.map((field) => scope[field]);
  return { kind: "scope", id: `scope:${encodeURIComponent(JSON.stringify(values))}` };
}

export function selectionReferenceForEvent(event: DiagnosticEvent): SelectionReference {
  if (event.kind === "agent_message_delta" || event.kind === "agent_message_completed") {
    return messageSelectionReference(event.message_id);
  }
  if (event.scope.tool_call_id !== null) {
    return scopeFieldSelectionReference("tool_call_id", event.scope.tool_call_id);
  }
  return eventSelectionReference(event.sequence);
}

function referenceValue(reference: SelectionReference, prefix: string): string | null {
  return reference.id.startsWith(prefix) ? reference.id.slice(prefix.length) : null;
}

function decodeFullScope(reference: SelectionReference): DiagnosticScope | null {
  const encoded = referenceValue(reference, "scope:");
  if (reference.kind !== "scope" || encoded === null) {
    return null;
  }
  try {
    const values = JSON.parse(decodeURIComponent(encoded)) as unknown;
    if (!Array.isArray(values) || values.length !== SCOPE_FIELDS.length) {
      return null;
    }
    const [
      sceneId,
      actorId,
      cueId,
      effectId,
      actId,
      toolCallId,
      sessionGeneration,
    ] = values;
    if (
      ![sceneId, actorId, cueId, effectId, actId, toolCallId]
        .every((value) => value === null || typeof value === "string")
      || (sessionGeneration !== null && typeof sessionGeneration !== "string")
    ) {
      return null;
    }
    return {
      scene_id: sceneId as string | null,
      actor_id: actorId as string | null,
      cue_id: cueId as string | null,
      effect_id: effectId as string | null,
      act_id: actId as string | null,
      tool_call_id: toolCallId as string | null,
      session_generation: sessionGeneration as U64String | null,
    };
  } catch {
    return null;
  }
}

function sameScope(left: DiagnosticScope, right: DiagnosticScope): boolean {
  return SCOPE_FIELDS.every((field) => left[field] === right[field]);
}

function scopeReferenceMatches(scope: DiagnosticScope, reference: SelectionReference): boolean {
  if (reference.kind !== "scope") {
    return false;
  }
  const fullScope = decodeFullScope(reference);
  if (fullScope !== null) {
    return sameScope(scope, fullScope);
  }
  for (const field of SCOPE_FIELDS.slice(0, 6) as readonly ScopeStringField[]) {
    const value = referenceValue(reference, `${field}:`);
    if (value !== null) {
      return scope[field] === value;
    }
  }
  return false;
}

function isSpanEvent(event: DiagnosticEvent, spanId: string): boolean {
  if (event.kind === "span_started" || event.kind === "custom_span_started") {
    return event.sequence === spanId;
  }
  return (event.kind === "span_finished" || event.kind === "custom_span_finished")
    && event.span_id === spanId;
}

function isMessageEvent(event: DiagnosticEvent, messageId: string): boolean {
  return (event.kind === "agent_message_delta" || event.kind === "agent_message_completed")
    && event.message_id === messageId;
}

function eventDirectlyMatches(
  event: DiagnosticEvent,
  reference: SelectionReference,
): boolean {
  if (reference.kind === "event") {
    return event.sequence === referenceValue(reference, "sequence:");
  }
  if (reference.kind === "span") {
    const spanId = referenceValue(reference, "span:");
    return spanId !== null && isSpanEvent(event, spanId);
  }
  if (reference.kind === "message") {
    return isMessageEvent(event, reference.id);
  }
  return scopeReferenceMatches(event.scope, reference);
}

function rangeForEvents(events: readonly DiagnosticEvent[]): SelectionElapsedRange {
  let start = events[0]!.elapsed_ns;
  let end = start;
  for (const event of events.slice(1)) {
    if (BigInt(event.elapsed_ns) < BigInt(start)) {
      start = event.elapsed_ns;
    }
    if (BigInt(event.elapsed_ns) > BigInt(end)) {
      end = event.elapsed_ns;
    }
  }
  return { start_ns: start, end_ns: end };
}

export function resolveSelection(
  reference: SelectionReference | null,
  boundedEvents: readonly DiagnosticEvent[],
): ResolvedSelection | null {
  if (reference === null) {
    return null;
  }
  const matched = boundedEvents.filter((event) => eventDirectlyMatches(event, reference));
  if (matched.length === 0) {
    return null;
  }
  return {
    reference,
    scope: matched[0]!.scope,
    elapsed_range: rangeForEvents(matched),
    event_sequences: matched.map((event) => event.sequence),
  };
}

function sharesOperationalScope(left: DiagnosticScope, right: DiagnosticScope): boolean {
  if (left.act_id !== null && right.act_id !== null) {
    return left.act_id === right.act_id;
  }
  if (left.cue_id !== null && right.cue_id !== null) {
    return left.cue_id === right.cue_id;
  }
  if (left.actor_id !== null && right.actor_id !== null) {
    return left.actor_id === right.actor_id;
  }
  return left.scene_id !== null
    && right.scene_id !== null
    && left.scene_id === right.scene_id;
}

function elapsedWithin(elapsed: U64String, range: SelectionElapsedRange): boolean {
  const value = BigInt(elapsed);
  return value >= BigInt(range.start_ns) && value <= BigInt(range.end_ns);
}

function causallyRelated(
  event: DiagnosticEvent,
  resolved: ResolvedSelection,
  boundedEvents: readonly DiagnosticEvent[],
): boolean {
  const selected = new Set(resolved.event_sequences);
  if (event.caused_by.some((link) => selected.has(link.source_sequence))) {
    return true;
  }
  return boundedEvents.some((candidate) => (
    selected.has(candidate.sequence)
    && candidate.caused_by.some((link) => link.source_sequence === event.sequence)
  ));
}

export function eventSelectionHighlight(
  event: DiagnosticEvent,
  reference: SelectionReference | null,
  boundedEvents: readonly DiagnosticEvent[],
): SelectionHighlight {
  if (reference === null) {
    return "none";
  }
  if (eventDirectlyMatches(event, reference)) {
    return "selected";
  }
  const resolved = resolveSelection(reference, boundedEvents);
  if (resolved === null) {
    return "none";
  }
  return causallyRelated(event, resolved, boundedEvents)
    || (
      elapsedWithin(event.elapsed_ns, resolved.elapsed_range)
      && sharesOperationalScope(event.scope, resolved.scope)
    )
    ? "related"
    : "none";
}

function sparseScopeContains(container: DiagnosticScope, value: DiagnosticScope): boolean {
  let constrained = false;
  for (const field of SCOPE_FIELDS) {
    if (container[field] !== null) {
      constrained = true;
      if (container[field] !== value[field]) {
        return false;
      }
    }
  }
  return constrained;
}

export function selectionHighlightsScope(
  scope: DiagnosticScope,
  reference: SelectionReference | null,
  boundedEvents: readonly DiagnosticEvent[],
): boolean {
  if (reference !== null && scopeReferenceMatches(scope, reference)) {
    return true;
  }
  const resolved = resolveSelection(reference, boundedEvents);
  return resolved !== null && (
    sparseScopeContains(scope, resolved.scope)
    || sparseScopeContains(resolved.scope, scope)
    || sharesOperationalScope(scope, resolved.scope)
  );
}

export function selectionOverlapsElapsedRange(
  range: SelectionElapsedRange,
  reference: SelectionReference | null,
  boundedEvents: readonly DiagnosticEvent[],
): boolean {
  const resolved = resolveSelection(reference, boundedEvents);
  return resolved !== null
    && BigInt(range.start_ns) <= BigInt(resolved.elapsed_range.end_ns)
    && BigInt(range.end_ns) >= BigInt(resolved.elapsed_range.start_ns);
}
