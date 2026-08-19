import { compareU64 } from "../protocol/decimal.ts";
import type { DiagnosticEvent } from "../protocol/event.ts";
import { createFixedLru, lruDelete, lruGet, lruSet } from "./lru.ts";
import type { EventWindow, LiveEdgeState, WindowState } from "./model.ts";
import {
  ADJACENT_WINDOW_CAPACITY,
  LIVE_EDGE_EVENT_CAPACITY,
  VISIBLE_WINDOW_EVENT_CAPACITY,
} from "./model.ts";


export function createWindowState(): WindowState {
  return {
    visible: null,
    adjacent: createFixedLru(ADJACENT_WINDOW_CAPACITY),
  };
}

export function createEventWindow(window: EventWindow): EventWindow {
  if (window.id.length === 0) {
    throw new RangeError("window identity must not be empty");
  }
  if (compareU64(window.start_ns, window.end_ns) > 0) {
    throw new RangeError("window range is reversed");
  }
  if (window.events.length > VISIBLE_WINDOW_EVENT_CAPACITY) {
    throw new RangeError("window event capacity exceeded");
  }
  let previous: DiagnosticEvent | undefined;
  for (const event of window.events) {
    if (event.run_id !== window.run_id) {
      throw new RangeError("window contains an event from another run");
    }
    if (compareU64(event.sequence, window.captured_through) > 0) {
      throw new RangeError("window event is newer than its captured watermark");
    }
    if (
      compareU64(event.elapsed_ns, window.start_ns) < 0
      || compareU64(event.elapsed_ns, window.end_ns) > 0
    ) {
      throw new RangeError("window event is outside its elapsed range");
    }
    if (previous !== undefined && compareU64(previous.sequence, event.sequence) >= 0) {
      throw new RangeError("window event sequence is not strictly ordered");
    }
    previous = event;
  }
  return {
    ...window,
    events: [...window.events],
  };
}

export function cacheAdjacentWindow(state: WindowState, window: EventWindow): WindowState {
  const candidate = createEventWindow(window);
  if (state.visible?.id === candidate.id) {
    return state;
  }
  return { ...state, adjacent: lruSet(state.adjacent, candidate.id, candidate).state };
}

export function activateWindow(state: WindowState, window: EventWindow): WindowState {
  const candidate = createEventWindow(window);
  if (state.visible?.id === candidate.id) {
    return { ...state, visible: candidate };
  }
  let adjacent = lruDelete(state.adjacent, candidate.id);
  if (state.visible !== null) {
    adjacent = lruSet(adjacent, state.visible.id, state.visible).state;
  }
  return { visible: candidate, adjacent };
}

export function promoteAdjacentWindow(state: WindowState, id: string): WindowState {
  const read = lruGet(state.adjacent, id);
  if (read.value === undefined) {
    return state;
  }
  return activateWindow({ ...state, adjacent: lruDelete(read.state, id) }, read.value);
}

export function appendLiveEvent(
  state: LiveEdgeState,
  event: DiagnosticEvent,
  projection: LiveEdgeState["projection"],
): LiveEdgeState {
  let droppedThrough = state.dropped_through;
  let events: DiagnosticEvent[];
  if (state.events.length >= LIVE_EDGE_EVENT_CAPACITY) {
    // Keep the immutable edge bounded with one copy. Repeated shift() calls
    // turn every high-rate event into an O(capacity) series of moves.
    droppedThrough = state.events[0]?.sequence ?? droppedThrough;
    events = state.events.slice(1);
    events.push(event);
  } else {
    events = [...state.events, event];
  }
  return {
    ...state,
    events,
    dropped_through: droppedThrough,
    observed_elapsed_ns: compareU64(event.elapsed_ns, state.observed_elapsed_ns) > 0
      ? event.elapsed_ns
      : state.observed_elapsed_ns,
    projection,
  };
}
