import {
  Brain,
  CircleDot,
  Wrench,
} from "lucide-preact";
import type { JSX } from "preact";

import type { U64String } from "../protocol/decimal.ts";
import type {
  DiagnosticScope,
  SpanStartedEvent,
} from "../protocol/event.ts";
import type {
  ProjectedResultFact,
  ProjectedSpan,
  ProjectedToolFact,
  SelectionReference,
} from "../state/model.ts";
import {
  eventReference,
  sameSelectionReference,
  spanReference,
} from "../state/selection.ts";


type ThinkingSpanStart = SpanStartedEvent & { readonly span_kind: "agent.thinking" };
type ThinkingSpan = ProjectedSpan & { readonly start: ThinkingSpanStart };

export type ToolResultItem =
  | { readonly kind: "thinking"; readonly span: ThinkingSpan }
  | {
    readonly kind: "tool";
    readonly fact: ProjectedToolFact;
    readonly started_elapsed_ns: U64String | null;
    readonly latest_for_tool: boolean;
  }
  | { readonly kind: "result"; readonly fact: ProjectedResultFact };

export interface ToolResultRowsProps {
  readonly items: readonly ToolResultItem[];
  readonly observedElapsedNs: U64String;
  readonly selection: SelectionReference | null;
  readonly onSelectionChange?: ((selection: SelectionReference) => void) | undefined;
}

export interface ToolResultRowProps {
  readonly item: ToolResultItem;
  readonly observedElapsedNs: U64String;
  readonly selection: SelectionReference | null;
  readonly onSelectionChange?: ((selection: SelectionReference) => void) | undefined;
}

function compareSequence(left: U64String, right: U64String): number {
  const leftValue = BigInt(left);
  const rightValue = BigInt(right);
  return leftValue < rightValue ? -1 : leftValue > rightValue ? 1 : 0;
}

function isThinkingSpan(span: ProjectedSpan): span is ThinkingSpan {
  return span.start?.kind === "span_started" && span.start.span_kind === "agent.thinking";
}

function scopeIdentity(scope: DiagnosticScope): string {
  return JSON.stringify([
    scope.scene_id,
    scope.actor_id,
    scope.cue_id,
    scope.act_id,
    scope.session_generation,
  ]);
}

function toolIdentity(fact: ProjectedToolFact): string {
  if (fact.span_id !== null) {
    return `span:${fact.span_id}`;
  }
  if (fact.tool_call_id !== null) {
    return `call:${scopeIdentity(fact.scope)}:${fact.tool_call_id}`;
  }
  return `event:${fact.sequence}`;
}

export function selectToolResultItems(
  spans: readonly ProjectedSpan[],
  tools: readonly ProjectedToolFact[],
  results: readonly ProjectedResultFact[],
): readonly ToolResultItem[] {
  const orderedTools = [...tools].sort((left, right) => compareSequence(left.sequence, right.sequence));
  const startedByTool = new Map<string, U64String>();
  const latestByTool = new Map<string, U64String>();
  for (const fact of orderedTools) {
    const identity = toolIdentity(fact);
    if (fact.phase === "started" && !startedByTool.has(identity)) {
      startedByTool.set(identity, fact.elapsed_ns);
    }
    latestByTool.set(identity, fact.sequence);
  }

  const items: ToolResultItem[] = [
    ...spans.filter(isThinkingSpan).map((span): ToolResultItem => ({
      kind: "thinking",
      span,
    })),
    ...orderedTools.map((fact): ToolResultItem => {
      const identity = toolIdentity(fact);
      return {
        kind: "tool",
        fact,
        started_elapsed_ns: startedByTool.get(identity) ?? null,
        latest_for_tool: latestByTool.get(identity) === fact.sequence,
      };
    }),
    ...results.map((fact): ToolResultItem => ({ kind: "result", fact })),
  ];
  return items.sort((left, right) => compareSequence(
    toolResultSequence(left),
    toolResultSequence(right),
  ));
}

export function toolResultScope(item: ToolResultItem): DiagnosticScope {
  if (item.kind === "thinking") {
    return item.span.start.scope;
  }
  return item.fact.scope;
}

export function toolResultSequence(item: ToolResultItem): U64String {
  return item.kind === "thinking" ? item.span.start.sequence : item.fact.sequence;
}

export function toolResultElapsed(item: ToolResultItem): U64String {
  if (item.kind === "thinking") {
    return item.span.finish?.elapsed_ns ?? item.span.start.elapsed_ns;
  }
  return item.fact.elapsed_ns;
}

export function toolResultKey(item: ToolResultItem): string {
  return item.kind === "thinking"
    ? `thinking:${item.span.span_id}`
    : `${item.kind}:${item.fact.sequence}`;
}

function duration(start: U64String, end: U64String): string {
  const startNs = BigInt(start);
  const endNs = BigInt(end);
  return `${endNs >= startNs ? endNs - startNs : 0n} ns`;
}

function resultLabel(kind: ProjectedResultFact["result_kind"]): string {
  switch (kind) {
    case "result.submitted":
      return "Result submitted";
    case "result.rejected":
      return "Result rejected";
    case "result.repair_requested":
      return "Result repair requested";
    case "result.accepted":
      return "Result accepted";
    case "result.missing":
      return "Result missing";
  }
}

function SelectButton({
  label,
  reference,
  kind,
  onSelectionChange,
}: {
  readonly label: string;
  readonly reference: SelectionReference;
  readonly kind: "thinking" | "tool" | "result";
  readonly onSelectionChange: ((selection: SelectionReference) => void) | undefined;
}): JSX.Element {
  const Icon = kind === "thinking" ? Brain : kind === "tool" ? Wrench : CircleDot;
  return (
    <button
      type="button"
      class="transcript-select-button"
      aria-label={label}
      title={label}
      disabled={onSelectionChange === undefined}
      onClick={() => onSelectionChange?.(reference)}
    >
      <Icon aria-hidden="true" size={17} strokeWidth={1.75} />
    </button>
  );
}

function ThinkingRow({
  span,
  observedElapsedNs,
  selection,
  onSelectionChange,
}: {
  readonly span: ThinkingSpan;
  readonly observedElapsedNs: U64String;
  readonly selection: SelectionReference | null;
  readonly onSelectionChange: ((selection: SelectionReference) => void) | undefined;
}): JSX.Element {
  const reference = spanReference(span.span_id);
  const selected = sameSelectionReference(selection, reference);
  const finish = span.finish;
  return (
    <article
      class="transcript-activity"
      data-activity-kind="thinking"
      data-selected={selected}
    >
      <header class="transcript-row-header">
        <SelectButton
          label={`Select thinking span ${span.span_id}`}
          reference={reference}
          kind="thinking"
          onSelectionChange={onSelectionChange}
        />
        <div class="transcript-row-heading"><h4>Thinking</h4><span>Span {span.span_id}</span></div>
        <span class="transcript-status" data-status={finish?.outcome ?? "running"}>
          {finish?.outcome ?? "Running"}
        </span>
      </header>
      <dl class="transcript-metadata">
        <div>
          <dt>Duration</dt>
          <dd>{duration(span.start.elapsed_ns, finish?.elapsed_ns ?? observedElapsedNs)}</dd>
        </div>
      </dl>
    </article>
  );
}

function ToolFactRow({
  item,
  observedElapsedNs,
  selection,
  onSelectionChange,
}: {
  readonly item: Extract<ToolResultItem, { readonly kind: "tool" }>;
  readonly observedElapsedNs: U64String;
  readonly selection: SelectionReference | null;
  readonly onSelectionChange: ((selection: SelectionReference) => void) | undefined;
}): JSX.Element {
  const { fact } = item;
  const reference = eventReference(fact.sequence);
  const selected = sameSelectionReference(selection, reference);
  const status = fact.status ?? fact.outcome ?? fact.phase;
  const durationEnd = item.latest_for_tool
    && (fact.status === "pending" || fact.status === "in_progress")
    ? observedElapsedNs
    : fact.elapsed_ns;
  return (
    <article
      class="transcript-activity"
      data-activity-kind="tool"
      data-tool-phase={fact.phase}
      data-selected={selected}
    >
      <header class="transcript-row-header">
        <SelectButton
          label={`Select tool ${fact.phase} event ${fact.sequence}`}
          reference={reference}
          kind="tool"
          onSelectionChange={onSelectionChange}
        />
        <div class="transcript-row-heading">
          <h4>{fact.title ?? "Tool call"}</h4>
          <span>{fact.tool_kind ?? "Unknown"} / {fact.phase}</span>
        </div>
        <span class="transcript-status" data-status={status}>{status}</span>
      </header>
      <dl class="transcript-metadata">
        <div><dt>Tool call</dt><dd>{fact.tool_call_id ?? "Unknown"}</dd></div>
        <div><dt>Sequence</dt><dd>{fact.sequence}</dd></div>
        {item.started_elapsed_ns === null
          ? <div><dt>Duration</dt><dd>Unknown</dd></div>
          : <div><dt>Duration</dt><dd>{duration(item.started_elapsed_ns, durationEnd)}</dd></div>}
        {fact.outcome === null ? null : <div><dt>Outcome</dt><dd>{fact.outcome}</dd></div>}
        {fact.error_code === null ? null : <div><dt>Error code</dt><dd>{fact.error_code}</dd></div>}
      </dl>
    </article>
  );
}

function ResultFactRow({
  fact,
  selection,
  onSelectionChange,
}: {
  readonly fact: ProjectedResultFact;
  readonly selection: SelectionReference | null;
  readonly onSelectionChange: ((selection: SelectionReference) => void) | undefined;
}): JSX.Element {
  const reference = eventReference(fact.sequence);
  const selected = sameSelectionReference(selection, reference);
  return (
    <article
      class="transcript-activity"
      data-activity-kind="result"
      data-selected={selected}
    >
      <header class="transcript-row-header">
        <SelectButton
          label={`Select ${fact.result_kind} event ${fact.sequence}`}
          reference={reference}
          kind="result"
          onSelectionChange={onSelectionChange}
        />
        <div class="transcript-row-heading">
          <h4>{resultLabel(fact.result_kind)}</h4>
          <span>Sequence {fact.sequence}</span>
        </div>
      </header>
      <dl class="transcript-metadata">
        {fact.issue === null ? null : <div><dt>Issue</dt><dd>{fact.issue.code}</dd></div>}
        {fact.issue === null ? null : <div><dt>Path</dt><dd>{fact.issue.path}</dd></div>}
        {fact.error_code === null ? null : <div><dt>Error code</dt><dd>{fact.error_code}</dd></div>}
        {fact.issue === null && fact.error_code === null
          ? <div><dt>Metadata</dt><dd>None</dd></div>
          : null}
      </dl>
    </article>
  );
}

export function ToolResultRow({
  item,
  observedElapsedNs,
  selection,
  onSelectionChange,
}: ToolResultRowProps): JSX.Element {
  if (item.kind === "thinking") {
    return (
      <ThinkingRow
        span={item.span}
        observedElapsedNs={observedElapsedNs}
        selection={selection}
        onSelectionChange={onSelectionChange}
      />
    );
  }
  if (item.kind === "tool") {
    return (
      <ToolFactRow
        item={item}
        observedElapsedNs={observedElapsedNs}
        selection={selection}
        onSelectionChange={onSelectionChange}
      />
    );
  }
  return (
    <ResultFactRow
      fact={item.fact}
      selection={selection}
      onSelectionChange={onSelectionChange}
    />
  );
}

export function ToolResultRows({
  items,
  observedElapsedNs,
  selection,
  onSelectionChange,
}: ToolResultRowsProps): JSX.Element {
  return (
    <div class="transcript-tool-result-rows">
      {items.map((item) => (
        <ToolResultRow
          key={toolResultKey(item)}
          item={item}
          observedElapsedNs={observedElapsedNs}
          selection={selection}
          onSelectionChange={onSelectionChange}
        />
      ))}
    </div>
  );
}
