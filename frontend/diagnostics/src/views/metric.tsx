import type { JSX } from "preact";

import type {
  JsonObject,
  JsonValue,
} from "../protocol/decimal.ts";
import type {
  MetricViewRecord,
  MetricViewResponse,
  ViewCoverage,
} from "../protocol/view.ts";


export interface MetricViewLocalError {
  readonly code: string;
  readonly message: string;
}

export type MetricViewState =
  | { readonly status: "loading" }
  | { readonly status: "ready"; readonly response: MetricViewResponse }
  | { readonly status: "local_error"; readonly error: MetricViewLocalError };

export interface MetricViewProps {
  readonly record: MetricViewRecord;
  readonly state: MetricViewState;
}

type ReadyState =
  | "ready"
  | "empty"
  | "partial"
  | "unavailable"
  | "gap"
  | "truncated"
  | "incompatible"
  | "corrupt";

function ExactNumber({ value }: { readonly value: string }): JSX.Element {
  return <code class="metric-view__number">{value}</code>;
}

function jsonObject(value: JsonValue | undefined): JsonObject | null {
  return value !== null && typeof value === "object" && !Array.isArray(value)
    ? value as JsonObject
    : null;
}

function groupText(group: JsonObject | null): string {
  if (group === null) {
    return "All facts";
  }
  const dimension = jsonObject(group.dimension);
  const scalar = jsonObject(group.value);
  const name = typeof dimension?.dimension === "string" ? dimension.dimension : "group";
  const value = scalar?.value;
  return `${name}: ${value === null || value === undefined ? "Unknown" : String(value)}`;
}

function AggregateValue({ value }: { readonly value: JsonObject | null }): JSX.Element {
  if (value === null) {
    return <span class="metric-view__unknown">Unavailable</span>;
  }
  if (value.aggregate === "exact") {
    const number = jsonObject(value.value);
    return typeof number?.value === "string"
      ? <ExactNumber value={number.value} />
      : <span class="metric-view__unknown">Unavailable</span>;
  }
  const numerator = jsonObject(value.numerator);
  return typeof numerator?.value === "string" && typeof value.contributing_count === "string" ? (
    <span class="metric-view__mean">
      <ExactNumber value={numerator.value} />
      <span> / </span>
      <ExactNumber value={value.contributing_count} />
    </span>
  ) : <span class="metric-view__unknown">Unavailable</span>;
}

function Coverage({
  coverage,
  label,
}: {
  readonly coverage: ViewCoverage;
  readonly label: string;
}): JSX.Element {
  return (
    <section class="metric-view__coverage" aria-label={label} data-coverage={coverage.status}>
      <h4>Coverage</h4>
      <dl>
        <div><dt>Status</dt><dd>{coverage.status}</dd></div>
        <div><dt>Matched</dt><dd><ExactNumber value={coverage.matched_count} /></dd></div>
        <div><dt>Contributing</dt><dd><ExactNumber value={coverage.contributing_count} /></dd></div>
        <div><dt>Excluded</dt><dd><ExactNumber value={coverage.excluded_count} /></dd></div>
        <div><dt>Open spans</dt><dd><ExactNumber value={coverage.excluded.open_spans} /></dd></div>
        <div><dt>Missing values</dt><dd><ExactNumber value={coverage.excluded.missing_values} /></dd></div>
        <div><dt>Non-numeric values</dt><dd><ExactNumber value={coverage.excluded.non_numeric_values} /></dd></div>
        <div><dt>Unavailable values</dt><dd><ExactNumber value={coverage.excluded.unavailable_values} /></dd></div>
        <div><dt>Resource truncated</dt><dd><ExactNumber value={coverage.excluded.resource_truncated} /></dd></div>
        <div><dt>Observation gaps</dt><dd><ExactNumber value={coverage.gap_count} /></dd></div>
      </dl>
    </section>
  );
}

function Binding({ response }: { readonly response: MetricViewResponse }): JSX.Element {
  const { binding } = response;
  return (
    <section class="metric-view__binding" aria-label="Metric binding">
      <h4>Binding</h4>
      <dl>
        <div><dt>Time</dt><dd>{binding.time_range === "viewport" ? "Viewport" : "Run"}</dd></div>
        <div>
          <dt>Range</dt>
          <dd>
            <ExactNumber value={binding.range_start_ns} /> to <ExactNumber value={binding.range_end_ns} /> ns
          </dd>
        </div>
        <div><dt>Scope</dt><dd>{binding.scope === "selection" ? "Selection" : "Run"}</dd></div>
        <div><dt>Captured watermark</dt><dd><ExactNumber value={binding.captured_watermark} /></dd></div>
        <div>
          <dt>Captured elapsed end</dt>
          <dd><ExactNumber value={binding.captured_elapsed_end_ns} /> ns</dd>
        </div>
      </dl>
    </section>
  );
}

function readyState(response: MetricViewResponse): ReadyState {
  if (response.incompatible?.reason === "newer_view_schema") {
    return "incompatible";
  }
  if (response.incompatible?.reason === "corrupt_record") {
    return "corrupt";
  }
  if (response.truncated) {
    return "truncated";
  }
  if (response.coverage.gap_count !== "0") {
    return "gap";
  }
  if (response.coverage.status === "partial") {
    return "partial";
  }
  if (response.coverage.status === "unavailable") {
    return "unavailable";
  }
  return response.series.length === 0 || response.series.every((series) => series.value === null)
    ? "empty"
    : "ready";
}

function Notices({ response }: { readonly response: MetricViewResponse }): JSX.Element {
  const incompatible = response.incompatible;
  if (incompatible !== null) {
    return incompatible.reason === "newer_view_schema" ? (
      <p role="status">
        This metric requires view schema {incompatible.record_view_schema_version}; this UI
        supports schema {incompatible.supported_view_schema_version}.
      </p>
    ) : (
      <p role="alert">
        The stored metric view record is corrupt. Record schema: {incompatible.record_view_schema_version
          ?? <span class="metric-view__unknown">Unknown</span>}.
      </p>
    );
  }
  return (
    <div class="metric-view__notices" aria-label="Metric result state">
      {response.coverage.status === "partial" ? <p role="status">Coverage is partial.</p> : null}
      {response.coverage.status === "unavailable" ? <p role="status">Coverage is unavailable.</p> : null}
      {response.coverage.gap_count === "0" ? null : (
        <p role="status">
          Observation gaps affect this metric: <ExactNumber value={response.coverage.gap_count} />.
        </p>
      )}
      {response.truncated ? (
        <p role="status">
          The metric result is truncated: <ExactNumber
            value={response.coverage.excluded.resource_truncated}
          /> excluded values.
        </p>
      ) : null}
    </div>
  );
}

function Series({ response }: { readonly response: MetricViewResponse }): JSX.Element {
  if (response.incompatible !== null) {
    return <p class="metric-view__unavailable">Metric series are unavailable.</p>;
  }
  if (response.series.length === 0 || response.series.every((series) => series.value === null)) {
    return <p class="metric-view__empty">No metric values in the captured result.</p>;
  }
  return (
    <div class="metric-view__series">
      {response.series.map((series, index) => (
        <article
          class="metric-view__series-item"
          data-testid={`metric-series-${index}`}
          key={`${JSON.stringify(series.group)}:${series.unit ?? ""}`}
        >
          <header>
            <h4 class="metric-view__business-text">{groupText(series.group)}</h4>
            <span class="metric-view__unit">{series.unit ?? "Unitless"}</span>
          </header>
          <p class="metric-view__value"><AggregateValue value={series.value} /></p>
          {series.value?.aggregate === "mean" ? (
            <p class="metric-view__mean-label">Exact numerator / contributing count</p>
          ) : null}
          <Coverage coverage={series.coverage} label={`Metric series ${index + 1} coverage`} />
        </article>
      ))}
    </div>
  );
}

function Result({ response }: { readonly response: MetricViewResponse }): JSX.Element {
  return (
    <div
      class="metric-view__result"
      data-state={readyState(response)}
      data-gap={response.coverage.gap_count === "0" ? "false" : "true"}
      data-truncated={response.truncated ? "true" : "false"}
    >
      <Notices response={response} />
      <div class="metric-view__metadata">
        <Binding response={response} />
        <Coverage coverage={response.coverage} label="Metric coverage" />
      </div>
      <Series response={response} />
    </div>
  );
}

export function MetricView({ record, state }: MetricViewProps): JSX.Element {
  const headingId = `metric-view-${record.id}-title`;
  return (
    <section
      class="metric-view"
      aria-labelledby={headingId}
      aria-busy={state.status === "loading"}
      data-state={state.status === "ready" ? readyState(state.response) : state.status}
      data-time-binding={state.status === "ready" ? state.response.binding.time_range : record.time_range}
      data-scope-binding={state.status === "ready" ? state.response.binding.scope : record.scope}
      style={{ letterSpacing: 0, minWidth: 0, overflowWrap: "anywhere", wordBreak: "break-word" }}
    >
      <header class="metric-view__header">
        <div><p>Metric view</p><h3 id={headingId}>{record.title}</h3></div>
        <code>{record.id}</code>
      </header>
      {state.status === "loading" ? (
        <p role="status">Loading metric view.</p>
      ) : state.status === "local_error" ? (
        <div class="metric-view__local-error" role="alert">
          <strong>Metric view failed</strong>
          <code>{state.error.code}</code>
          <p>{state.error.message}</p>
        </div>
      ) : <Result response={state.response} />}
    </section>
  );
}
