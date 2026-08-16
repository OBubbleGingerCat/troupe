import type { JSX } from "preact";

import type {
  JsonObject,
  JsonValue,
  U64String,
} from "../protocol/decimal.ts";
import type {
  TimeSeriesViewRecord,
  TimeSeriesViewResponse,
  ViewCoverage,
} from "../protocol/view.ts";


export interface TimeSeriesLocalError {
  readonly code: string;
  readonly message: string;
}

export interface ArchiveViewUnavailable {
  readonly reason: "newer_view_schema" | "corrupt_record";
  readonly supported_view_schema_version: 1;
  readonly record_view_schema_version: number | null;
}

export type TimeSeriesInteractiveState =
  | { readonly status: "loading" }
  | { readonly status: "ready"; readonly response: TimeSeriesViewResponse }
  | { readonly status: "local_error"; readonly error: TimeSeriesLocalError };

export type TimeSeriesShellProps =
  | {
    readonly record: TimeSeriesViewRecord;
    readonly state: TimeSeriesInteractiveState;
  }
  | {
    readonly record: TimeSeriesViewRecord | null;
    readonly state: { readonly status: "archive_unavailable"; readonly unavailable: ArchiveViewUnavailable };
  };

export interface TimeSeriesColumnarSeries {
  readonly group: JsonObject | null;
  readonly bucket_start_ns: readonly U64String[];
  readonly bucket_end_ns: readonly U64String[];
  readonly partial: readonly boolean[];
  readonly values: readonly (JsonObject | null)[];
  readonly coverage: readonly ViewCoverage[];
}

export interface TimeSeriesMountModel {
  readonly range_start_ns: U64String;
  readonly range_end_ns: U64String;
  readonly captured_watermark: U64String;
  readonly bucket_width_ns: U64String;
  readonly series: readonly TimeSeriesColumnarSeries[];
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

export function selectTimeSeriesMount(
  response: TimeSeriesViewResponse,
): TimeSeriesMountModel | null {
  if (response.incompatible !== null) {
    return null;
  }
  return {
    range_start_ns: response.binding.range_start_ns,
    range_end_ns: response.binding.range_end_ns,
    captured_watermark: response.binding.captured_watermark,
    bucket_width_ns: response.bucket_width_ns,
    series: response.series.map((series) => ({
      group: series.group,
      bucket_start_ns: series.points.map((point) => point.bucket_start_ns),
      bucket_end_ns: series.points.map((point) => point.bucket_end_ns),
      partial: series.points.map((point) => point.partial),
      values: series.points.map((point) => point.value),
      coverage: series.points.map((point) => point.coverage),
    })),
  };
}

function ExactNumber({ value }: { readonly value: string }): JSX.Element {
  return <code class="timeseries-shell__number">{value}</code>;
}

function readyState(response: TimeSeriesViewResponse): ReadyState {
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
  return response.series.length === 0 || response.series.every((series) => series.points.length === 0)
    ? "empty"
    : "ready";
}

function Coverage({ coverage }: { readonly coverage: ViewCoverage }): JSX.Element {
  return (
    <section
      class="timeseries-shell__coverage"
      aria-label="Time series coverage"
      data-coverage={coverage.status}
    >
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

function Binding({ response }: { readonly response: TimeSeriesViewResponse }): JSX.Element {
  const { binding } = response;
  return (
    <section class="timeseries-shell__binding" aria-label="Time series binding">
      <h4>Binding</h4>
      <dl>
        <div><dt>Time</dt><dd>{binding.time_range === "viewport" ? "Viewport" : "Run"}</dd></div>
        <div>
          <dt>Range</dt>
          <dd><ExactNumber value={binding.range_start_ns} /> to <ExactNumber value={binding.range_end_ns} /> ns</dd>
        </div>
        <div><dt>Scope</dt><dd>{binding.scope === "selection" ? "Selection" : "Run"}</dd></div>
        <div><dt>Captured watermark</dt><dd><ExactNumber value={binding.captured_watermark} /></dd></div>
        <div><dt>Bucket width</dt><dd><ExactNumber value={response.bucket_width_ns} /> ns</dd></div>
      </dl>
    </section>
  );
}

function jsonObject(value: JsonValue | undefined): JsonObject | null {
  return value !== null && typeof value === "object" && !Array.isArray(value)
    ? value as JsonObject
    : null;
}

function groupText(group: JsonObject | null): string {
  if (group === null) {
    return "All series";
  }
  const dimension = group.dimension;
  const value = group.value;
  const dimensionObject = jsonObject(dimension);
  const valueObject = jsonObject(value);
  const dimensionName = typeof dimensionObject?.dimension === "string"
    ? dimensionObject.dimension
    : "group";
  const groupValue = valueObject !== null && "value" in valueObject
    ? valueObject.value
    : null;
  return `${dimensionName}: ${groupValue === null ? "Unknown" : String(groupValue)}`;
}

function Notices({ response }: { readonly response: TimeSeriesViewResponse }): JSX.Element {
  const incompatible = response.incompatible;
  if (incompatible !== null) {
    return incompatible.reason === "newer_view_schema" ? (
      <p role="status">
        This time series requires view schema {incompatible.record_view_schema_version}; this UI
        supports schema {incompatible.supported_view_schema_version}.
      </p>
    ) : (
      <p role="alert">
        The stored time series view record is corrupt. Record schema: {incompatible.record_view_schema_version
          ?? <span class="timeseries-shell__unknown">Unknown</span>}.
      </p>
    );
  }
  return (
    <div class="timeseries-shell__notices" aria-label="Time series result state">
      {response.coverage.status === "partial" ? <p role="status">Coverage is partial.</p> : null}
      {response.coverage.status === "unavailable" ? <p role="status">Coverage is unavailable.</p> : null}
      {response.coverage.gap_count === "0" ? null : (
        <p role="status">Observation gaps affect this series: <ExactNumber value={response.coverage.gap_count} />.</p>
      )}
      {response.truncated ? (
        <p role="status">
          The result is truncated: <ExactNumber value={response.coverage.excluded.resource_truncated} />
          excluded values.
        </p>
      ) : null}
    </div>
  );
}

function PlotMount({ response }: { readonly response: TimeSeriesViewResponse }): JSX.Element {
  const model = selectTimeSeriesMount(response);
  if (model === null) {
    return <p class="timeseries-shell__unavailable">Time series data are unavailable.</p>;
  }
  const pointCount = model.series.reduce((total, series) => total + series.values.length, 0);
  if (model.series.length === 0 || pointCount === 0) {
    return <p class="timeseries-shell__empty">No time series points in the captured range.</p>;
  }
  return (
    <section
      class="timeseries-shell__plot-mount"
      aria-label="Time series plot"
      data-series-count={String(model.series.length)}
      data-point-count={String(pointCount)}
      data-bucket-width-ns={model.bucket_width_ns}
    >
      <h4>Series</h4>
      <ol>
        {model.series.map((series, index) => (
          <li key={index} class="timeseries-shell__business-text">
            <span>{groupText(series.group)}</span>
            <span>{series.values.length} aligned points</span>
          </li>
        ))}
      </ol>
    </section>
  );
}

function Result({ response }: { readonly response: TimeSeriesViewResponse }): JSX.Element {
  return (
    <div
      class="timeseries-shell__result"
      data-state={readyState(response)}
      data-gap={response.coverage.gap_count === "0" ? "false" : "true"}
      data-truncated={response.truncated ? "true" : "false"}
    >
      <Notices response={response} />
      <div class="timeseries-shell__metadata">
        <Binding response={response} />
        <Coverage coverage={response.coverage} />
      </div>
      <PlotMount response={response} />
    </div>
  );
}

function ArchiveUnavailable({ unavailable }: { readonly unavailable: ArchiveViewUnavailable }): JSX.Element {
  return (
    <div class="timeseries-shell__archive-unavailable" role="status">
      <strong>Archived time series unavailable</strong>
      {unavailable.reason === "newer_view_schema" ? (
        <p>
          The record uses view schema {unavailable.record_view_schema_version}; this UI supports
          schema {unavailable.supported_view_schema_version}.
        </p>
      ) : (
        <p>
          The stored view record is corrupt. Record schema: {unavailable.record_view_schema_version
            ?? <span class="timeseries-shell__unknown">Unknown</span>}.
        </p>
      )}
    </div>
  );
}

export function TimeSeriesShell(props: TimeSeriesShellProps): JSX.Element {
  const headingId = props.record === null
    ? "timeseries-shell-unavailable-title"
    : `timeseries-shell-${props.record.id}-title`;
  const title = props.record?.title ?? "Archived time series";
  const state = props.state.status === "ready"
    ? readyState(props.state.response)
    : props.state.status;
  return (
    <section
      class="timeseries-shell"
      aria-labelledby={headingId}
      aria-busy={props.state.status === "loading"}
      data-state={state}
      style={{ letterSpacing: 0, minWidth: 0, overflowWrap: "anywhere", wordBreak: "break-word" }}
    >
      <header class="timeseries-shell__header">
        <div><p>Time series view</p><h3 id={headingId}>{title}</h3></div>
        {props.record === null ? null : <code>{props.record.id}</code>}
      </header>
      {props.state.status === "loading" ? (
        <p role="status">Loading time series view.</p>
      ) : props.state.status === "local_error" ? (
        <div class="timeseries-shell__local-error" role="alert">
          <strong>Time series view failed</strong>
          <code>{props.state.error.code}</code>
          <p>{props.state.error.message}</p>
        </div>
      ) : props.state.status === "archive_unavailable" ? (
        <ArchiveUnavailable unavailable={props.state.unavailable} />
      ) : <Result response={props.state.response} />}
    </section>
  );
}
