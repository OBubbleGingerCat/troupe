import type { SpanStartedEvent } from "../protocol/event.ts";
import type {
  DiagnosticState,
  ProjectedSpan,
  SelectionReference,
} from "../state/model.ts";
import { presentedLiveEdge } from "../state/reducer.ts";


export type ExecutionNodeKind =
  | "production"
  | "scene"
  | "actor"
  | "cue"
  | "wait"
  | "execution"
  | "act"
  | "tool";

export type ExecutionStatus =
  | "queued"
  | "waiting"
  | "running"
  | "completed"
  | "failed"
  | "cancelled"
  | "partial";

export interface ActorCueSummary {
  readonly done: number;
  readonly running: number;
  readonly queued: number;
}

export interface CueStageSummary {
  readonly wait: ExecutionStatus;
  readonly execution: ExecutionStatus;
}

export interface ExecutionNode {
  readonly key: string;
  readonly kind: ExecutionNodeKind;
  readonly label: string;
  readonly secondaryLabel: string | null;
  readonly status: ExecutionStatus | null;
  readonly selection: SelectionReference;
  readonly selected: boolean;
  readonly expandable: boolean;
  readonly expanded: boolean;
  readonly actorSummary: ActorCueSummary | null;
  readonly cueStages: CueStageSummary | null;
  readonly children: readonly ExecutionNode[];
}

export interface ExecutionTreeModel {
  readonly root: ExecutionNode;
  readonly needsServerRefresh: boolean;
}

export interface ShellReadout {
  readonly deliveredThrough: string;
  readonly committedWatermark: string;
  readonly paused: boolean;
  readonly unseenCount: string;
}

type BuiltInSpan = ProjectedSpan & { readonly start: SpanStartedEvent };

interface CueBuilder {
  readonly id: string;
  readonly spans: BuiltInSpan[];
}

interface ActorBuilder {
  readonly id: string;
  readonly spans: BuiltInSpan[];
  readonly cues: Map<string, CueBuilder>;
}

interface SceneBuilder {
  readonly id: string;
  readonly spans: BuiltInSpan[];
  readonly actors: Map<string, ActorBuilder>;
}

function isBuiltInSpan(span: ProjectedSpan): span is BuiltInSpan {
  return span.start?.kind === "span_started";
}

function spanOrder(left: BuiltInSpan, right: BuiltInSpan): number {
  const leftSequence = BigInt(left.start.sequence);
  const rightSequence = BigInt(right.start.sequence);
  return leftSequence < rightSequence ? -1 : leftSequence > rightSequence ? 1 : 0;
}

function firstSpan(
  spans: readonly BuiltInSpan[],
  kind: SpanStartedEvent["span_kind"],
): BuiltInSpan | null {
  return spans.filter((span) => span.start.span_kind === kind).sort(spanOrder)[0] ?? null;
}

function spansOfKind(
  spans: readonly BuiltInSpan[],
  kind: SpanStartedEvent["span_kind"],
): readonly BuiltInSpan[] {
  return spans.filter((span) => span.start.span_kind === kind).sort(spanOrder);
}

function spanStatus(span: BuiltInSpan | null): ExecutionStatus {
  if (span === null) {
    return "partial";
  }
  return span.finish?.outcome ?? "running";
}

function selectionMatches(
  state: DiagnosticState,
  selection: SelectionReference,
): boolean {
  return state.presentation.selection?.kind === selection.kind
    && state.presentation.selection.id === selection.id;
}

function scopeKey(kind: "production" | "scene" | "actor" | "cue", ...ids: string[]): string {
  return JSON.stringify([kind, ...ids]);
}

function scopeSelection(id: string): SelectionReference {
  return { kind: "scope", id };
}

function spanSelection(span: BuiltInSpan): SelectionReference {
  return { kind: "span", id: span.span_id };
}

function leafNode(
  state: DiagnosticState,
  span: BuiltInSpan,
  kind: "wait" | "execution" | "tool",
  label: string,
  secondaryLabel: string | null = null,
): ExecutionNode {
  const selection = spanSelection(span);
  const status = kind === "wait" && span.finish === null
    ? "waiting"
    : spanStatus(span);
  return {
    key: JSON.stringify([kind, span.span_id]),
    kind,
    label,
    secondaryLabel,
    status,
    selection,
    selected: selectionMatches(state, selection),
    expandable: false,
    expanded: false,
    actorSummary: null,
    cueStages: null,
    children: [],
  };
}

function detailString(span: BuiltInSpan, field: string): string | null {
  const value = span.start.detail[field];
  return typeof value === "string" && value.length > 0 ? value : null;
}

function actNode(
  state: DiagnosticState,
  span: BuiltInSpan,
  tools: readonly BuiltInSpan[],
): ExecutionNode {
  const actId = span.start.scope.act_id ?? `sequence-${span.start.sequence}`;
  const selection = spanSelection(span);
  const children = [...tools]
    .sort(spanOrder)
    .map((tool) => leafNode(
      state,
      tool,
      "tool",
      detailString(tool, "title") ?? `Tool ${tool.start.scope.tool_call_id ?? tool.span_id}`,
      tool.start.scope.tool_call_id,
    ));
  return {
    key: JSON.stringify(["act", span.span_id]),
    kind: "act",
    label: `Act ${actId}`,
    secondaryLabel: detailString(span, "effective_model"),
    status: spanStatus(span),
    selection,
    selected: selectionMatches(state, selection),
    expandable: false,
    expanded: true,
    actorSummary: null,
    cueStages: null,
    children,
  };
}

function cueStatus(
  wait: BuiltInSpan | null,
  execution: BuiltInSpan | null,
  acts: readonly BuiltInSpan[],
): ExecutionStatus {
  if (execution?.finish !== null && execution?.finish !== undefined) {
    return execution.finish.outcome;
  }
  if (execution !== null || acts.length > 0) {
    return "running";
  }
  if (wait?.finish?.outcome === "failed" || wait?.finish?.outcome === "cancelled") {
    return wait.finish.outcome;
  }
  return "queued";
}

function cueNode(
  state: DiagnosticState,
  sceneId: string,
  actorId: string,
  cue: CueBuilder,
): ExecutionNode {
  const waits = spansOfKind(cue.spans, "cue.mailbox_wait");
  const executions = spansOfKind(cue.spans, "cue.execution");
  const acts = spansOfKind(cue.spans, "act.lifecycle");
  const tools = spansOfKind(cue.spans, "tool.call");
  const firstWait = waits[0] ?? null;
  const firstExecution = executions[0] ?? null;
  const actIds = new Set(acts.map((act) => act.start.scope.act_id));
  const children: ExecutionNode[] = [
    ...waits.map((wait) => leafNode(state, wait, "wait", "Mailbox wait")),
    ...executions.map((execution) => (
      leafNode(state, execution, "execution", "Actor.cued()")
    )),
    ...acts.map((act) => actNode(
      state,
      act,
      tools.filter((tool) => tool.start.scope.act_id === act.start.scope.act_id),
    )),
    ...tools
      .filter((tool) => !actIds.has(tool.start.scope.act_id))
      .map((tool) => leafNode(
        state,
        tool,
        "tool",
        detailString(tool, "title") ?? `Tool ${tool.start.scope.tool_call_id ?? tool.span_id}`,
        tool.start.scope.tool_call_id,
      )),
  ];
  const key = scopeKey("cue", sceneId, actorId, cue.id);
  const selection = scopeSelection(cue.id);
  const expanded = state.presentation.expanded.includes(key);
  return {
    key,
    kind: "cue",
    label: `Cue ${cue.id}`,
    secondaryLabel: null,
    status: cueStatus(firstWait, firstExecution, acts),
    selection,
    selected: selectionMatches(state, selection),
    expandable: children.length > 0,
    expanded,
    actorSummary: null,
    cueStages: {
      wait: firstWait === null
        ? "partial"
        : firstWait.finish === null ? "waiting" : spanStatus(firstWait),
      execution: firstExecution === null ? "queued" : spanStatus(firstExecution),
    },
    children,
  };
}

function summarizeCues(cues: readonly ExecutionNode[]): ActorCueSummary {
  let done = 0;
  let running = 0;
  let queued = 0;
  for (const cue of cues) {
    if (cue.status === "running") {
      running += 1;
    } else if (cue.status === "queued" || cue.status === "waiting" || cue.status === "partial") {
      queued += 1;
    } else {
      done += 1;
    }
  }
  return { done, running, queued };
}

function actorNode(
  state: DiagnosticState,
  sceneId: string,
  actor: ActorBuilder,
): ExecutionNode {
  const actorSpan = firstSpan(actor.spans, "actor.handle_lifetime");
  const children = [...actor.cues.values()]
    .map((cue) => cueNode(state, sceneId, actor.id, cue))
    .sort((left, right) => left.label.localeCompare(right.label));
  const key = scopeKey("actor", sceneId, actor.id);
  const selection = scopeSelection(actor.id);
  const displayName = actorSpan === null ? null : detailString(actorSpan, "display_name");
  return {
    key,
    kind: "actor",
    label: displayName ?? `Actor ${actor.id}`,
    secondaryLabel: displayName === null ? null : actor.id,
    status: null,
    selection,
    selected: selectionMatches(state, selection),
    expandable: false,
    expanded: true,
    actorSummary: summarizeCues(children),
    cueStages: null,
    children,
  };
}

function sceneNode(state: DiagnosticState, scene: SceneBuilder): ExecutionNode {
  const sceneSpan = firstSpan(scene.spans, "scene.lifecycle");
  const children = [...scene.actors.values()]
    .map((actor) => actorNode(state, scene.id, actor))
    .sort((left, right) => left.label.localeCompare(right.label));
  const key = scopeKey("scene", scene.id);
  const selection = scopeSelection(scene.id);
  return {
    key,
    kind: "scene",
    label: `Scene ${scene.id}`,
    secondaryLabel: null,
    status: sceneSpan === null && children.length > 0 ? "running" : spanStatus(sceneSpan),
    selection,
    selected: selectionMatches(state, selection),
    expandable: false,
    expanded: true,
    actorSummary: null,
    cueStages: null,
    children,
  };
}

function buildScenes(spans: readonly BuiltInSpan[]): readonly SceneBuilder[] {
  const scenes = new Map<string, SceneBuilder>();
  for (const span of spans) {
    const sceneId = span.start.scope.scene_id;
    if (sceneId === null) {
      continue;
    }
    let scene = scenes.get(sceneId);
    if (scene === undefined) {
      scene = { id: sceneId, spans: [], actors: new Map() };
      scenes.set(sceneId, scene);
    }
    scene.spans.push(span);

    const actorId = span.start.scope.actor_id;
    if (actorId === null) {
      continue;
    }
    let actor = scene.actors.get(actorId);
    if (actor === undefined) {
      actor = { id: actorId, spans: [], cues: new Map() };
      scene.actors.set(actorId, actor);
    }
    actor.spans.push(span);

    const cueId = span.start.scope.cue_id;
    if (cueId === null) {
      continue;
    }
    let cue = actor.cues.get(cueId);
    if (cue === undefined) {
      cue = { id: cueId, spans: [] };
      actor.cues.set(cueId, cue);
    }
    cue.spans.push(span);
  }
  return [...scenes.values()].sort((left, right) => left.id.localeCompare(right.id));
}

export function selectExecutionTree(
  state: DiagnosticState,
  productionName: string,
): ExecutionTreeModel {
  const edge = presentedLiveEdge(state);
  const spans = edge.projection.spans.items.filter(isBuiltInSpan);
  const runSpan = firstSpan(spans, "run.lifecycle");
  const children = buildScenes(spans).map((scene) => sceneNode(state, scene));
  const key = scopeKey("production", state.run_id);
  const selection = scopeSelection(state.run_id);
  return {
    root: {
      key,
      kind: "production",
      label: productionName,
      secondaryLabel: "Production",
      status: runSpan === null && children.length > 0 ? "running" : spanStatus(runSpan),
      selection,
      selected: selectionMatches(state, selection),
      expandable: false,
      expanded: true,
      actorSummary: null,
      cueStages: null,
      children,
    },
    needsServerRefresh: edge.projection.spans.needs_server_refresh,
  };
}

export function selectShellReadout(state: DiagnosticState): ShellReadout {
  return {
    deliveredThrough: state.cursor.delivered_through,
    committedWatermark: state.cursor.committed_watermark,
    paused: state.pause.paused,
    unseenCount: state.pause.unseen_count.toString(),
  };
}
