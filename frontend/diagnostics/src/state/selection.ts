import {
  type U64String,
  compareU64,
  decodeU64,
} from "../protocol/decimal.ts";
import type { DiagnosticScope } from "../protocol/event.ts";
import type {
  PresentationFilters,
  PresentationState,
  SelectionReference,
} from "./model.ts";
import { EXPANDED_ITEM_CAPACITY } from "./model.ts";


const SCOPE_REFERENCE_FIELDS = [
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

export function eventReference(sequence: U64String): SelectionReference {
  return { kind: "event", id: sequence };
}

export function spanReference(spanId: U64String): SelectionReference {
  return { kind: "span", id: spanId };
}

export function messageReference(messageId: string): SelectionReference {
  if (messageId.length === 0) {
    throw new RangeError("message selection identity must not be empty");
  }
  return { kind: "message", id: messageId };
}

export function scopeReference(scope: DiagnosticScope): SelectionReference {
  return {
    kind: "scope",
    id: JSON.stringify(SCOPE_REFERENCE_FIELDS.map((field) => scope[field])),
  };
}

export function hierarchyScope(
  scope: DiagnosticScope,
  through: ScopeHierarchyField,
): DiagnosticScope {
  const scene: DiagnosticScope = {
    scene_id: scope.scene_id,
    actor_id: null,
    cue_id: null,
    effect_id: null,
    act_id: null,
    tool_call_id: null,
    session_generation: null,
  };
  if (through === "scene_id") {
    return scene;
  }
  const actor = { ...scene, actor_id: scope.actor_id };
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
  return { ...act, tool_call_id: scope.tool_call_id };
}

export function hierarchyScopeReference(
  scope: DiagnosticScope,
  through: ScopeHierarchyField,
): SelectionReference {
  return scopeReference(hierarchyScope(scope, through));
}

export function scopeFromReference(reference: SelectionReference): DiagnosticScope | null {
  if (reference.kind !== "scope") {
    return null;
  }
  try {
    const values = JSON.parse(reference.id) as unknown;
    if (!Array.isArray(values) || values.length !== SCOPE_REFERENCE_FIELDS.length) {
      return null;
    }
    const [sceneId, actorId, cueId, effectId, actId, toolCallId, sessionGeneration] = values;
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
      session_generation: sessionGeneration === null
        ? null
        : decodeU64(sessionGeneration, "selection.scope.session_generation"),
    };
  } catch {
    return null;
  }
}

export function sameSelectionReference(
  left: SelectionReference | null,
  right: SelectionReference | null,
): boolean {
  return left?.kind === right?.kind && left?.id === right?.id;
}

export function createPresentationState(): PresentationState {
  return {
    selection: null,
    pinned_detail: null,
    expanded: [],
    filters: { event_kinds: [], scene_id: null, actor_id: null, text: "" },
    viewport: null,
    follow_live: true,
    zoom: null,
  };
}

export function select(
  state: PresentationState,
  selection: SelectionReference | null,
): PresentationState {
  if (sameSelectionReference(state.selection, selection)) {
    return state;
  }
  return { ...state, selection };
}

export function pinDetail(
  state: PresentationState,
  selection: SelectionReference | null,
): PresentationState {
  if (sameSelectionReference(state.pinned_detail, selection)) {
    return state;
  }
  return { ...state, pinned_detail: selection };
}

export function toggleExpanded(state: PresentationState, id: string): PresentationState {
  if (id.length === 0) {
    throw new RangeError("expanded identity must not be empty");
  }
  if (state.expanded.includes(id)) {
    return { ...state, expanded: state.expanded.filter((candidate) => candidate !== id) };
  }
  return {
    ...state,
    expanded: [...state.expanded, id].slice(-EXPANDED_ITEM_CAPACITY),
  };
}

export function setFilters(
  state: PresentationState,
  filters: PresentationFilters,
): PresentationState {
  return { ...state, filters };
}

export function setViewport(
  state: PresentationState,
  viewport: PresentationState["viewport"],
): PresentationState {
  if (viewport !== null && compareU64(viewport.start_ns, viewport.end_ns) > 0) {
    throw new RangeError("viewport range is reversed");
  }
  return { ...state, viewport };
}

export function setFollowLive(state: PresentationState, followLive: boolean): PresentationState {
  return state.follow_live === followLive ? state : { ...state, follow_live: followLive };
}

export function setZoom(
  state: PresentationState,
  zoom: PresentationState["zoom"],
): PresentationState {
  if (zoom !== null && (!Number.isFinite(zoom.scale) || zoom.scale <= 0)) {
    throw new RangeError("zoom scale must be finite and positive");
  }
  return { ...state, zoom };
}
