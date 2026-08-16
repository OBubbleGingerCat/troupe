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
import {
  eventReference,
  messageReference,
  sameSelectionReference,
  scopeFromReference,
  scopeReference,
  spanReference,
} from "../state/selection.ts";


const SCOPE_FIELDS = [
  "scene_id",
  "actor_id",
  "cue_id",
  "effect_id",
  "act_id",
  "tool_call_id",
  "session_generation",
] as const;

export type ScopeHierarchyField =
  | "scene_id"
  | "actor_id"
  | "cue_id"
  | "effect_id"
  | "act_id"
  | "tool_call_id";

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

export function hierarchyScope(
  scope: DiagnosticScope,
  through: ScopeHierarchyField,
): DiagnosticScope {
  const base: DiagnosticScope = {
    scene_id: scope.scene_id,
    actor_id: null,
    cue_id: null,
    effect_id: null,
    act_id: null,
    tool_call_id: null,
    session_generation: scope.session_generation,
  };
  if (through === "scene_id") {
    return base;
  }
  const actor = { ...base, actor_id: scope.actor_id };
  if (through === "actor_id") {
    return actor;
  }
  const cue = { ...actor, cue_id: scope.cue_id };
  if (through === "cue_id") {
    return cue;
  }
  if (through === "effect_id") {
    return { ...cue, effect_id: scope.effect_id };
  }
  const act = { ...cue, act_id: scope.act_id };
  if (through === "act_id") {
    return act;
  }
  return {
    ...act,
    effect_id: scope.effect_id,
    tool_call_id: scope.tool_call_id,
  };
}

export function hierarchyScopeReference(
  scope: DiagnosticScope,
  through: ScopeHierarchyField,
): SelectionReference {
  return scopeReference(hierarchyScope(scope, through));
}

export function selectionReferenceForEvent(event: DiagnosticEvent): SelectionReference {
  if (event.kind === "agent_message_delta" || event.kind === "agent_message_completed") {
    return messageReference(event.message_id);
  }
  if (event.scope.tool_call_id !== null) {
    return hierarchyScopeReference(event.scope, "tool_call_id");
  }
  return eventReference(event.sequence);
}

function sameScope(left: DiagnosticScope, right: DiagnosticScope): boolean {
  return SCOPE_FIELDS.every((field) => left[field] === right[field]);
}

function scopeReferenceMatches(scope: DiagnosticScope, reference: SelectionReference): boolean {
  const selectedScope = scopeFromReference(reference);
  return selectedScope !== null
    && (sameScope(scope, selectedScope) || sparseScopeContains(selectedScope, scope));
}

function spanEventReference(event: DiagnosticEvent): SelectionReference | null {
  if (event.kind === "span_started" || event.kind === "custom_span_started") {
    return spanReference(event.sequence);
  }
  return event.kind === "span_finished" || event.kind === "custom_span_finished"
    ? spanReference(event.span_id)
    : null;
}

function messageEventReference(event: DiagnosticEvent): SelectionReference | null {
  return event.kind === "agent_message_delta" || event.kind === "agent_message_completed"
    ? messageReference(event.message_id)
    : null;
}

function eventDirectlyMatches(
  event: DiagnosticEvent,
  reference: SelectionReference,
): boolean {
  if (reference.kind === "event") {
    return sameSelectionReference(eventReference(event.sequence), reference);
  }
  if (reference.kind === "span") {
    return sameSelectionReference(spanEventReference(event), reference);
  }
  if (reference.kind === "message") {
    return sameSelectionReference(messageEventReference(event), reference);
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
