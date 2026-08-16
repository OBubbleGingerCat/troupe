import type { JSX } from "preact";

import { compareU64 } from "../protocol/decimal.ts";
import type {
  ActTokenUsageFinalizedEvent,
  DiagnosticScope,
} from "../protocol/event.ts";
import type {
  DiagnosticState,
  ProjectedActUsage,
  ProjectedContextUsage,
  ProjectedSpan,
} from "../state/model.ts";
import { presentedLiveEdge } from "../state/reducer.ts";
import { scopeFromReference } from "../state/selection.ts";
import { ContextMeter } from "./ContextMeter.tsx";
import {
  type ValidatedUsageAggregate,
  UsageCoverage,
} from "./UsageCoverage.tsx";
import {
  formatTokenCount,
  formatUnavailableReason,
} from "./format.ts";
import "./usage.css";


const TOKEN_FIELDS = [
  ["provider_total_tokens", "Provider total"],
  ["input_tokens", "Input"],
  ["output_tokens", "Output"],
  ["thought_tokens", "Thought"],
  ["cached_read_tokens", "Cached read"],
  ["cached_write_tokens", "Cached write"],
] as const;

const SCOPE_FIELDS = [
  "scene_id",
  "actor_id",
  "cue_id",
  "effect_id",
  "act_id",
  "tool_call_id",
  "session_generation",
] as const;

export interface UsagePanelProps {
  readonly state: DiagnosticState;
  readonly validatedAggregates?: readonly ValidatedUsageAggregate[];
}

type ActAccountingRow =
  | { readonly kind: "pending"; readonly span: ProjectedSpan }
  | { readonly kind: "finalized"; readonly usage: ProjectedActUsage };

function matchesScope(candidate: DiagnosticScope, selected: DiagnosticScope | null): boolean {
  return selected === null || SCOPE_FIELDS.every((field) => (
    selected[field] === null || selected[field] === candidate[field]
  ));
}

function selectedScope(
  state: DiagnosticState,
  spans: readonly ProjectedSpan[],
): DiagnosticScope | null {
  const selection = state.presentation.selection;
  if (selection === null) {
    return null;
  }
  const direct = scopeFromReference(selection);
  if (direct !== null) {
    return direct;
  }
  if (selection.kind !== "span") {
    return null;
  }
  const span = spans.find((candidate) => candidate.span_id === selection.id);
  return span?.start?.scope ?? span?.finish?.scope ?? null;
}

function latestContext(
  samples: readonly ProjectedContextUsage[],
  scope: DiagnosticScope | null,
): ProjectedContextUsage | null {
  let latest: ProjectedContextUsage | null = null;
  for (const sample of samples) {
    if (
      matchesScope(sample.event.scope, scope)
      && (latest === null || compareU64(latest.event.sequence, sample.event.sequence) < 0)
    ) {
      latest = sample;
    }
  }
  return latest;
}

function accountingRows(
  spans: readonly ProjectedSpan[],
  usages: readonly ProjectedActUsage[],
  scope: DiagnosticScope | null,
): readonly ActAccountingRow[] {
  const rows: ActAccountingRow[] = usages
    .filter((usage) => matchesScope(usage.event.scope, scope))
    .map((usage) => ({ kind: "finalized", usage }));
  const finalizedActIds = new Set(rows.map((row) => (
    row.kind === "finalized" ? row.usage.event.scope.act_id : null
  )));

  for (const span of spans) {
    if (
      span.start?.kind === "span_started"
      && span.start.span_kind === "act.lifecycle"
      && span.finish === null
      && span.start.scope.act_id !== null
      && matchesScope(span.start.scope, scope)
      && !finalizedActIds.has(span.start.scope.act_id)
    ) {
      rows.push({ kind: "pending", span });
    }
  }

  return rows.sort((left, right) => {
    const leftSequence = left.kind === "finalized"
      ? left.usage.event.sequence
      : left.span.start!.sequence;
    const rightSequence = right.kind === "finalized"
      ? right.usage.event.sequence
      : right.span.start!.sequence;
    return compareU64(rightSequence, leftSequence);
  });
}

function Availability({ event }: { readonly event: ActTokenUsageFinalizedEvent }): JSX.Element {
  return (
    <div class="usage-act__metadata">
      <span class={`usage-status usage-status--${event.availability}`}>
        {event.availability[0]!.toUpperCase()}{event.availability.slice(1)}
      </span>
      <dl>
        <div>
          <dt>Source</dt>
          <dd>{event.source ?? "Unknown"}</dd>
        </div>
        <div>
          <dt>Reason</dt>
          <dd>
            {event.unavailable_reason === null
              ? "Not applicable"
              : formatUnavailableReason(event.unavailable_reason)}
          </dd>
        </div>
      </dl>
    </div>
  );
}

function FinalizedAct({ usage }: { readonly usage: ProjectedActUsage }): JSX.Element {
  const { event } = usage;
  const actId = event.scope.act_id ?? usage.act_key;
  return (
    <article class="usage-act" data-testid={`act-accounting-${actId}`}>
      <header class="usage-act__header">
        <div>
          <p class="usage-eyebrow">Final Act accounting</p>
          <h3>{actId}</h3>
        </div>
        <Availability event={event} />
      </header>
      <dl class="usage-token-grid">
        {TOKEN_FIELDS.map(([field, label]) => (
          <div key={field}>
            <dt>{label}</dt>
            <dd class="usage-number">{formatTokenCount(event[field])}</dd>
          </div>
        ))}
      </dl>
    </article>
  );
}

function PendingAct({ span }: { readonly span: ProjectedSpan }): JSX.Element {
  const actId = span.start?.scope.act_id ?? `span-${span.span_id}`;
  return (
    <article class="usage-act usage-act--pending" data-testid={`act-accounting-${actId}`}>
      <header class="usage-act__header">
        <div>
          <p class="usage-eyebrow">Act in progress</p>
          <h3>{actId}</h3>
        </div>
        <span class="usage-status usage-status--pending">Pending</span>
      </header>
      <p class="usage-act__pending-copy">Final token accounting has not been reported.</p>
    </article>
  );
}

export function UsagePanel({
  state,
  validatedAggregates = [],
}: UsagePanelProps): JSX.Element {
  const edge = presentedLiveEdge(state);
  const scope = selectedScope(state, edge.projection.spans.items);
  const context = latestContext(edge.projection.context_usage.items, scope);
  const rows = accountingRows(
    edge.projection.spans.items,
    edge.projection.act_usage.items,
    scope,
  );
  const needsRefresh = edge.projection.context_usage.needs_server_refresh
    || edge.projection.act_usage.needs_server_refresh
    || edge.projection.spans.needs_server_refresh;

  return (
    <div class="usage-panel">
      {needsRefresh ? (
        <div class="usage-refresh-notice" role="status">
          This live projection is incomplete. Refresh from the diagnostic server for older facts.
        </div>
      ) : null}

      <ContextMeter sample={context} />

      <section class="usage-section usage-accounting" aria-labelledby="usage-accounting-heading">
        <header class="usage-section__header">
          <div>
            <p class="usage-eyebrow">Per-turn provider reports</p>
            <h2 id="usage-accounting-heading">Final Act accounting</h2>
          </div>
        </header>
        {rows.length === 0 ? (
          <p class="usage-empty">No Act accounting is present in the current projection.</p>
        ) : (
          <div class="usage-act-list">
            {rows.map((row) => row.kind === "finalized" ? (
              <FinalizedAct key={`finalized:${row.usage.act_key}`} usage={row.usage} />
            ) : (
              <PendingAct key={`pending:${row.span.span_id}`} span={row.span} />
            ))}
          </div>
        )}
      </section>

      <UsageCoverage aggregates={validatedAggregates} />
    </div>
  );
}
