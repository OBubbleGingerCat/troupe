import type { JSX } from "preact";

import { compareU64 } from "../protocol/decimal.ts";
import type { UsageFieldAggregateSnapshot } from "../protocol/http.ts";
import type { SelectedUsageAggregate } from "../state/model.ts";
import {
  formatCoverage,
  formatExactInteger,
  formatTokenCount,
} from "./format.ts";


const USAGE_FIELDS = [
  ["provider_total_tokens", "Provider total"],
  ["input_tokens", "Input"],
  ["output_tokens", "Output"],
  ["thought_tokens", "Thought"],
  ["cached_read_tokens", "Cached read"],
  ["cached_write_tokens", "Cached write"],
] as const;

export interface UsageCoverageProps {
  readonly aggregates: readonly SelectedUsageAggregate[];
}

function scopeName(kind: SelectedUsageAggregate["scope_kind"]): string {
  return `${kind[0]!.toUpperCase()}${kind.slice(1)}`;
}

function coverageLabel(field: UsageFieldAggregateSnapshot): string {
  if (field.known_sum === null) {
    return "Total unavailable";
  }
  if (field.finalized_acts === "0") {
    return "No finalized Acts";
  }
  return compareU64(field.reported_acts, field.finalized_acts) === 0
    ? "Complete known total"
    : "Known partial total";
}

function Aggregate({ selected }: { readonly selected: SelectedUsageAggregate }): JSX.Element {
  const kind = scopeName(selected.scope_kind);
  const aggregate = selected.aggregate;
  return (
    <article
      class="usage-aggregate"
      data-testid={`usage-aggregate-${selected.scope_kind}`}
    >
      <header class="usage-aggregate__header">
        <div>
          <p class="usage-eyebrow">{kind}</p>
          <h3>{selected.scope_label}</h3>
        </div>
        <span class="usage-aggregate__reported">
          {formatExactInteger(aggregate.reported_acts)} reported
        </span>
      </header>

      <dl class="usage-availability" aria-label={`${kind} accounting availability`}>
        <div><dt>Finalized</dt><dd>{formatExactInteger(aggregate.finalized_acts)}</dd></div>
        <div><dt>Reported</dt><dd>{formatExactInteger(aggregate.reported_acts)}</dd></div>
        <div><dt>Available</dt><dd>{formatExactInteger(aggregate.available_acts)}</dd></div>
        <div><dt>Partial</dt><dd>{formatExactInteger(aggregate.partial_acts)}</dd></div>
        <div><dt>Unavailable</dt><dd>{formatExactInteger(aggregate.unavailable_acts)}</dd></div>
      </dl>

      <div class="usage-coverage__table-wrap">
        <table>
          <thead>
            <tr>
              <th scope="col">Field</th>
              <th scope="col">Known sum</th>
              <th scope="col">Coverage</th>
              <th scope="col">Interpretation</th>
            </tr>
          </thead>
          <tbody>
            {USAGE_FIELDS.map(([field, label]) => {
              const fact = aggregate[field];
              return (
                <tr key={field}>
                  <th scope="row">{label}</th>
                  <td class="usage-number">{formatTokenCount(fact.known_sum)}</td>
                  <td>{formatCoverage(fact.reported_acts, fact.finalized_acts)}</td>
                  <td>{coverageLabel(fact)}</td>
                </tr>
              );
            })}
          </tbody>
        </table>
      </div>
    </article>
  );
}

export function UsageCoverage({ aggregates }: UsageCoverageProps): JSX.Element {
  return (
    <section class="usage-section usage-coverage" aria-labelledby="usage-coverage-heading">
      <header class="usage-section__header">
        <div>
          <p class="usage-eyebrow">Validated server aggregates</p>
          <h2 id="usage-coverage-heading">Usage coverage</h2>
        </div>
      </header>
      {aggregates.length === 0 ? (
        <p class="usage-empty">No aggregate snapshot is available for this scope.</p>
      ) : (
        <div class="usage-coverage__list">
          {aggregates.map((aggregate) => (
            <Aggregate
              key={`${aggregate.scope_kind}:${aggregate.scope_label}`}
              selected={aggregate}
            />
          ))}
        </div>
      )}
    </section>
  );
}
