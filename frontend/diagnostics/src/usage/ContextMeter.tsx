import type { JSX } from "preact";

import type { ProjectedContextUsage } from "../state/model.ts";
import {
  UNKNOWN_USAGE_VALUE,
  contextOccupancyPercent,
  formatU64Count,
} from "./format.ts";


export interface ContextMeterProps {
  readonly sample: ProjectedContextUsage | null;
}

export function ContextMeter({ sample }: ContextMeterProps): JSX.Element {
  if (sample === null) {
    return (
      <section class="usage-section usage-context" aria-labelledby="usage-context-heading">
        <header class="usage-section__header">
          <div>
            <p class="usage-eyebrow">Current session occupancy</p>
            <h2 id="usage-context-heading">Live context</h2>
          </div>
          <span class="usage-status usage-status--unknown">Unavailable</span>
        </header>
        <p class="usage-empty">No context sample is available in the current projection.</p>
      </section>
    );
  }

  const { event } = sample;
  const percent = contextOccupancyPercent(
    event.context_used_tokens,
    event.context_window_tokens,
  );
  const cost = event.cumulative_cost_amount === null
    ? UNKNOWN_USAGE_VALUE
    : `${event.cumulative_cost_currency ?? ""} ${event.cumulative_cost_amount}`.trim();

  return (
    <section class="usage-section usage-context" aria-labelledby="usage-context-heading">
      <header class="usage-section__header">
        <div>
          <p class="usage-eyebrow">Current session occupancy</p>
          <h2 id="usage-context-heading">Live context</h2>
        </div>
        <span class={`usage-status usage-status--${event.sample_origin}`}>
          {event.sample_origin === "provider" ? "Provider sample" : "Carried forward"}
        </span>
      </header>

      <div class="usage-context__meter">
        {percent === null ? (
          <div class="usage-context__track usage-context__track--unknown" aria-hidden="true" />
        ) : (
          <progress aria-label="Context occupancy" max={100} value={percent} />
        )}
        <div class="usage-context__reading">
          <strong>
            {formatU64Count(event.context_used_tokens)}
            <span aria-hidden="true"> / </span>
            <span class="usage-visually-hidden"> of </span>
            {formatU64Count(event.context_window_tokens)}
          </strong>
          <span>{percent === null ? "Occupancy unknown" : `${percent}% occupied`}</span>
        </div>
      </div>

      <dl class="usage-detail-grid">
        <div>
          <dt>Used tokens</dt>
          <dd>{formatU64Count(event.context_used_tokens)}</dd>
        </div>
        <div>
          <dt>Context window</dt>
          <dd>{formatU64Count(event.context_window_tokens)}</dd>
        </div>
        <div>
          <dt>Cumulative cost</dt>
          <dd>{cost}</dd>
        </div>
        <div>
          <dt>Observed elapsed ns</dt>
          <dd>{formatU64Count(event.observed_elapsed_ns)}</dd>
        </div>
      </dl>
    </section>
  );
}
