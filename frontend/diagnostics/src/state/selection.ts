import { compareU64 } from "../protocol/decimal.ts";
import type {
  PresentationFilters,
  PresentationState,
  SelectionReference,
} from "./model.ts";
import { EXPANDED_ITEM_CAPACITY } from "./model.ts";


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
  if (state.selection?.kind === selection?.kind && state.selection?.id === selection?.id) {
    return state;
  }
  return { ...state, selection };
}

export function pinDetail(
  state: PresentationState,
  selection: SelectionReference | null,
): PresentationState {
  if (state.pinned_detail?.kind === selection?.kind && state.pinned_detail?.id === selection?.id) {
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
