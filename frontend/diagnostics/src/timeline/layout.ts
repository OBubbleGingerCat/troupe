import type { U64String } from "../protocol/decimal.ts";
import type {
  DiagnosticScope,
  SpanStartedEvent,
} from "../protocol/event.ts";
import type {
  DiagnosticState,
  ProjectedSpan,
  SelectionReference,
} from "../state/model.ts";
import { presentedLiveEdge } from "../state/reducer.ts";
import {
  eventReference,
  hierarchyScopeReference,
  messageReference,
  scopeReference,
  spanReference,
} from "../state/selection.ts";
import {
  type TimelinePrimitive,
  type TimelineRowLanes,
  type TimelineTrackKind,
  assignTimelineLanes,
} from "./lanes.ts";


export const TIMELINE_ROW_HEIGHT = 32;
export const TIMELINE_OVERSCAN_ROWS = 2;

export type TimelineNodeKind =
  | "production"
  | "scene"
  | "actor"
  | "cue"
  | "act"
  | "caller"
  | "turn"
  | "tool"
  | "fact";

export type TimelineNodeStatus =
  | "queued"
  | "waiting"
  | "running"
  | "completed"
  | "failed"
  | "cancelled"
  | "partial";

export interface TimelineNode {
  readonly id: string;
  readonly parent_id: string | null;
  readonly kind: TimelineNodeKind;
  readonly label: string;
  readonly status: TimelineNodeStatus | null;
  readonly selection: SelectionReference;
  readonly expanded: boolean;
}

export interface TimelineModel {
  readonly nodes: readonly TimelineNode[];
  readonly primitives: readonly TimelinePrimitive[];
  readonly live_now_ns: U64String;
  readonly needs_server_refresh: boolean;
}

export interface TimelineVerticalViewport {
  readonly scroll_top: number;
  readonly height: number;
}

export interface TimelineRow {
  readonly node: TimelineNode;
  readonly index: number;
  readonly depth: number;
  readonly top: number;
  readonly height: number;
  readonly has_children: boolean;
}

export interface TimelineLayout {
  readonly model: TimelineModel;
  readonly rows: readonly TimelineRow[];
  readonly visible_rows: readonly TimelineRow[];
  readonly lanes_by_row: ReadonlyMap<string, TimelineRowLanes>;
  readonly total_height: number;
  readonly row_height: number;
  readonly scroll_top: number;
  readonly viewport_height: number;
}

type BuiltInSpan = ProjectedSpan & { readonly start: SpanStartedEvent };

interface MutableTimelineNode {
  id: string;
  parent_id: string | null;
  kind: TimelineNodeKind;
  label: string;
  status: TimelineNodeStatus | null;
  selection: SelectionReference;
  expanded: boolean;
}

interface ScopeNodeIds {
  readonly scene: string | null;
  readonly actor: string | null;
  readonly cue: string | null;
  readonly act: string | null;
  readonly tool: string | null;
  readonly most_specific: string;
}

const EMPTY_SCOPE: DiagnosticScope = {
  scene_id: null,
  actor_id: null,
  cue_id: null,
  effect_id: null,
  act_id: null,
  tool_call_id: null,
  session_generation: null,
};

function validateVerticalViewport(viewport: TimelineVerticalViewport): void {
  if (
    !Number.isFinite(viewport.scroll_top)
    || viewport.scroll_top < 0
    || !Number.isFinite(viewport.height)
    || viewport.height <= 0
  ) {
    throw new RangeError("timeline vertical viewport is invalid");
  }
}

export function layoutTimeline(
  model: TimelineModel,
  viewport: TimelineVerticalViewport,
): TimelineLayout {
  validateVerticalViewport(viewport);
  const nodes = new Map<string, TimelineNode>();
  const order = new Map<string, number>();
  model.nodes.forEach((node, index) => {
    if (nodes.has(node.id)) {
      throw new RangeError(`duplicate timeline node identity: ${node.id}`);
    }
    nodes.set(node.id, node);
    order.set(node.id, index);
  });
  const children = new Map<string | null, TimelineNode[]>();
  for (const node of model.nodes) {
    if (node.parent_id !== null && !nodes.has(node.parent_id)) {
      throw new RangeError(`timeline node has an unknown parent: ${node.id}`);
    }
    const siblings = children.get(node.parent_id);
    if (siblings === undefined) {
      children.set(node.parent_id, [node]);
    } else {
      siblings.push(node);
    }
  }
  for (const siblings of children.values()) {
    siblings.sort((left, right) => order.get(left.id)! - order.get(right.id)!);
  }

  const validating = new Set<string>();
  const validated = new Set<string>();
  const validateAncestry = (node: TimelineNode): void => {
    if (validated.has(node.id)) {
      return;
    }
    if (validating.has(node.id)) {
      throw new RangeError(`timeline hierarchy contains a cycle at ${node.id}`);
    }
    validating.add(node.id);
    if (node.parent_id !== null) {
      validateAncestry(nodes.get(node.parent_id)!);
    }
    validating.delete(node.id);
    validated.add(node.id);
  };
  for (const node of model.nodes) {
    validateAncestry(node);
  }

  const flattened: { readonly node: TimelineNode; readonly depth: number }[] = [];
  const visitVisible = (node: TimelineNode, depth: number): void => {
    flattened.push({ node, depth });
    if (node.expanded) {
      for (const child of children.get(node.id) ?? []) {
        visitVisible(child, depth + 1);
      }
    }
  };
  for (const root of children.get(null) ?? []) {
    visitVisible(root, 1);
  }

  const visibleNodeIds = new Set(flattened.map((item) => item.node.id));
  for (const primitive of model.primitives) {
    if (!nodes.has(primitive.row_id)) {
      throw new RangeError(`timeline primitive has an unknown row: ${primitive.id}`);
    }
  }
  const lanesByRow = assignTimelineLanes(
    model.primitives.filter((primitive) => visibleNodeIds.has(primitive.row_id)),
    model.live_now_ns,
  );
  const rows = flattened.map((item, index): TimelineRow => ({
    node: item.node,
    index,
    depth: item.depth,
    top: index * TIMELINE_ROW_HEIGHT,
    height: TIMELINE_ROW_HEIGHT,
    has_children: (children.get(item.node.id)?.length ?? 0) > 0,
  }));
  const first = Math.max(
    0,
    Math.floor(viewport.scroll_top / TIMELINE_ROW_HEIGHT) - TIMELINE_OVERSCAN_ROWS,
  );
  const last = Math.min(
    rows.length,
    Math.ceil((viewport.scroll_top + viewport.height) / TIMELINE_ROW_HEIGHT)
      + TIMELINE_OVERSCAN_ROWS,
  );
  return {
    model,
    rows,
    visible_rows: rows.slice(first, last),
    lanes_by_row: lanesByRow,
    total_height: rows.length * TIMELINE_ROW_HEIGHT,
    row_height: TIMELINE_ROW_HEIGHT,
    scroll_top: viewport.scroll_top,
    viewport_height: viewport.height,
  };
}

function isBuiltInSpan(span: ProjectedSpan): span is BuiltInSpan {
  return span.start?.kind === "span_started";
}

function nodeStatus(span: ProjectedSpan): TimelineNodeStatus {
  return span.finish?.outcome ?? "running";
}

function scopeKey(kind: TimelineNodeKind, ...parts: (string | null)[]): string {
  return JSON.stringify([kind, ...parts]);
}

function spanLabel(span: ProjectedSpan): string {
  const start = span.start;
  if (start === null) {
    return `Span ${span.span_id}`;
  }
  return start.kind === "span_started" ? start.span_kind : start.name;
}

function primitiveTrack(span: ProjectedSpan): TimelineTrackKind {
  if (!isBuiltInSpan(span)) {
    return "lifecycle";
  }
  if (span.start.span_kind === "act.caller") {
    return "caller";
  }
  if (span.start.span_kind === "agent.turn" || span.start.span_kind === "agent.thinking") {
    return "turn";
  }
  return "lifecycle";
}

function needsRefresh(state: DiagnosticState): boolean {
  const projection = presentedLiveEdge(state).projection;
  return [
    projection.spans,
    projection.messages,
    projection.counters,
    projection.context_usage,
    projection.act_usage,
    projection.tools,
    projection.results,
    projection.gaps,
  ].some((bucket) => bucket.needs_server_refresh);
}

export function selectTimelineModel(
  state: DiagnosticState,
  productionName: string,
): TimelineModel {
  const edge = presentedLiveEdge(state);
  const rootId = scopeKey("production", state.run_id);
  const mutableNodes = new Map<string, MutableTimelineNode>();
  const primitives: TimelinePrimitive[] = [];
  const expanded = new Set(state.presentation.expanded);

  const addNode = (node: MutableTimelineNode): string => {
    if (!mutableNodes.has(node.id)) {
      mutableNodes.set(node.id, node);
    }
    return node.id;
  };
  addNode({
    id: rootId,
    parent_id: null,
    kind: "production",
    label: productionName,
    status: "running",
    selection: scopeReference(EMPTY_SCOPE),
    expanded: true,
  });

  const ensureScope = (scope: DiagnosticScope): ScopeNodeIds => {
    let parent = rootId;
    let scene: string | null = null;
    let actor: string | null = null;
    let cue: string | null = null;
    let act: string | null = null;
    let tool: string | null = null;
    if (scope.scene_id !== null) {
      scene = scopeKey("scene", scope.scene_id);
      addNode({
        id: scene,
        parent_id: parent,
        kind: "scene",
        label: `Scene ${scope.scene_id}`,
        status: null,
        selection: hierarchyScopeReference(scope, "scene_id"),
        expanded: true,
      });
      parent = scene;
    }
    if (scope.actor_id !== null) {
      actor = scopeKey("actor", scope.scene_id, scope.actor_id);
      addNode({
        id: actor,
        parent_id: parent,
        kind: "actor",
        label: `Actor ${scope.actor_id}`,
        status: null,
        selection: hierarchyScopeReference(scope, "actor_id"),
        expanded: true,
      });
      parent = actor;
    }
    if (scope.cue_id !== null) {
      cue = scopeKey("cue", scope.scene_id, scope.actor_id, scope.cue_id);
      addNode({
        id: cue,
        parent_id: parent,
        kind: "cue",
        label: `Cue ${scope.cue_id}`,
        status: null,
        selection: hierarchyScopeReference(scope, "cue_id"),
        expanded: expanded.has(cue),
      });
      parent = cue;
    }
    if (scope.act_id !== null) {
      act = scopeKey("act", scope.scene_id, scope.actor_id, scope.cue_id, scope.act_id);
      addNode({
        id: act,
        parent_id: parent,
        kind: "act",
        label: `Act ${scope.act_id}`,
        status: null,
        selection: hierarchyScopeReference(scope, "act_id"),
        expanded: expanded.has(act),
      });
      parent = act;
    }
    if (scope.tool_call_id !== null) {
      tool = scopeKey(
        "tool",
        scope.scene_id,
        scope.actor_id,
        scope.cue_id,
        scope.act_id,
        scope.tool_call_id,
      );
      addNode({
        id: tool,
        parent_id: parent,
        kind: "tool",
        label: `Tool ${scope.tool_call_id}`,
        status: null,
        selection: hierarchyScopeReference(scope, "tool_call_id"),
        expanded: false,
      });
      parent = tool;
    }
    return { scene, actor, cue, act, tool, most_specific: parent };
  };

  for (const span of edge.projection.spans.items) {
    if (span.start === null) {
      continue;
    }
    const scope = span.start.scope;
    const scopeNodes = ensureScope(scope);
    let rowId = scopeNodes.most_specific;
    let structuralNode: string | null = null;
    if (isBuiltInSpan(span)) {
      switch (span.start.span_kind) {
        case "run.lifecycle":
          rowId = rootId;
          structuralNode = rootId;
          break;
        case "scene.lifecycle":
          rowId = scopeNodes.scene ?? rowId;
          structuralNode = rowId;
          break;
        case "actor.handle_lifetime":
          rowId = scopeNodes.actor ?? rowId;
          structuralNode = rowId;
          break;
        case "cue.mailbox_wait":
        case "cue.execution":
          rowId = scopeNodes.cue ?? rowId;
          structuralNode = rowId;
          break;
        case "act.lifecycle":
          rowId = scopeNodes.act ?? rowId;
          structuralNode = rowId;
          break;
        case "act.caller": {
          const id = scopeKey("caller", span.span_id);
          rowId = addNode({
            id,
            parent_id: scopeNodes.act ?? scopeNodes.most_specific,
            kind: "caller",
            label: "Act caller",
            status: nodeStatus(span),
            selection: spanReference(span.span_id),
            expanded: false,
          });
          break;
        }
        case "agent.turn": {
          const id = scopeKey("turn", span.span_id);
          rowId = addNode({
            id,
            parent_id: scopeNodes.act ?? scopeNodes.most_specific,
            kind: "turn",
            label: "Agent turn",
            status: nodeStatus(span),
            selection: spanReference(span.span_id),
            expanded: false,
          });
          break;
        }
        case "tool.call":
          rowId = scopeNodes.tool ?? rowId;
          structuralNode = rowId;
          break;
        default:
          break;
      }
    }
    if (structuralNode !== null) {
      const node = mutableNodes.get(structuralNode);
      if (node !== undefined) {
        node.status = nodeStatus(span);
      }
    }
    primitives.push({
      id: `span:${span.span_id}`,
      row_id: rowId,
      track: primitiveTrack(span),
      kind: "span",
      label: spanLabel(span),
      start_ns: span.start.elapsed_ns,
      end_ns: span.finish?.elapsed_ns ?? null,
      order: span.start.sequence,
      status: span.finish?.outcome ?? "running",
      selection: spanReference(span.span_id),
    });
  }

  for (const counter of edge.projection.counters.items) {
    const rowId = ensureScope(counter.event.scope).most_specific;
    primitives.push({
      id: `counter:${counter.event.sequence}`,
      row_id: rowId,
      track: "fact",
      kind: "counter",
      label: counter.event.kind === "counter_sampled" ? counter.event.counter_kind : counter.event.name,
      start_ns: counter.event.elapsed_ns,
      end_ns: counter.event.elapsed_ns,
      order: counter.event.sequence,
      status: null,
      selection: eventReference(counter.event.sequence),
    });
  }
  for (const message of edge.projection.messages.items) {
    const rowId = ensureScope(message.scope).most_specific;
    primitives.push({
      id: `message:${message.message_id}`,
      row_id: rowId,
      track: "fact",
      kind: "instant",
      label: "Agent message",
      start_ns: message.latest_elapsed_ns,
      end_ns: message.latest_elapsed_ns,
      order: message.latest_sequence,
      status: message.completion === null ? "running" : "completed",
      selection: messageReference(message.message_id),
    });
  }
  for (const usage of edge.projection.context_usage.items) {
    const rowId = ensureScope(usage.event.scope).most_specific;
    primitives.push({
      id: `context-usage:${usage.event.sequence}`,
      row_id: rowId,
      track: "fact",
      kind: "counter",
      label: "Context usage",
      start_ns: usage.event.elapsed_ns,
      end_ns: usage.event.elapsed_ns,
      order: usage.event.sequence,
      status: usage.event.sample_origin,
      selection: eventReference(usage.event.sequence),
    });
  }
  for (const usage of edge.projection.act_usage.items) {
    const rowId = ensureScope(usage.event.scope).most_specific;
    primitives.push({
      id: `act-usage:${usage.event.sequence}`,
      row_id: rowId,
      track: "fact",
      kind: "counter",
      label: "Act token usage",
      start_ns: usage.event.elapsed_ns,
      end_ns: usage.event.elapsed_ns,
      order: usage.event.sequence,
      status: usage.event.availability,
      selection: eventReference(usage.event.sequence),
    });
  }
  for (const fact of edge.projection.tools.items) {
    const scopeNodes = ensureScope(fact.scope);
    const rowId = scopeNodes.tool ?? scopeNodes.most_specific;
    const node = mutableNodes.get(rowId);
    if (node?.kind === "tool" && fact.status !== null) {
      node.status = fact.status === "in_progress" || fact.status === "pending"
        ? "running"
        : fact.status;
    }
    primitives.push({
      id: `tool-fact:${fact.sequence}`,
      row_id: rowId,
      track: "fact",
      kind: "instant",
      label: fact.title ?? `Tool ${fact.phase}`,
      start_ns: fact.elapsed_ns,
      end_ns: fact.elapsed_ns,
      order: fact.sequence,
      status: fact.status ?? fact.outcome ?? fact.phase,
      selection: fact.tool_call_id === null
        ? eventReference(fact.sequence)
        : hierarchyScopeReference(fact.scope, "tool_call_id"),
    });
  }
  for (const fact of edge.projection.results.items) {
    const rowId = ensureScope(fact.scope).most_specific;
    primitives.push({
      id: `result:${fact.sequence}`,
      row_id: rowId,
      track: "fact",
      kind: "instant",
      label: fact.result_kind,
      start_ns: fact.elapsed_ns,
      end_ns: fact.elapsed_ns,
      order: fact.sequence,
      status: fact.result_kind === "result.accepted" ? "completed" : null,
      selection: eventReference(fact.sequence),
    });
  }
  for (const gap of edge.projection.gaps.items) {
    const rowId = ensureScope(gap.affected_scope ?? gap.scope).most_specific;
    primitives.push({
      id: `gap:${gap.sequence}`,
      row_id: rowId,
      track: "fact",
      kind: "instant",
      label: "Observation gap",
      start_ns: gap.elapsed_ns,
      end_ns: gap.elapsed_ns,
      order: gap.sequence,
      status: "partial",
      selection: eventReference(gap.sequence),
    });
  }

  return {
    nodes: [...mutableNodes.values()],
    primitives,
    live_now_ns: edge.observed_elapsed_ns,
    needs_server_refresh: needsRefresh(state),
  };
}
