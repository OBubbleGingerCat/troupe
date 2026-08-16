import type { JSX } from "preact";

import type { JsonObject } from "../protocol/decimal.ts";
import type { TaggedScalar } from "../protocol/event.ts";
import type {
  TableViewRecord,
  TableViewResponse,
  ViewCoverage,
} from "../protocol/view.ts";


export interface TableViewLocalError {
  readonly code: string;
  readonly message: string;
}

export type TableViewState =
  | { readonly status: "loading" }
  | { readonly status: "ready"; readonly response: TableViewResponse }
  | { readonly status: "local_error"; readonly error: TableViewLocalError };

export interface TableViewProps {
  readonly record: TableViewRecord;
  readonly state: TableViewState;
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
  return <code class="table-view__number">{value}</code>;
}

function columnLabel(column: JsonObject): string {
  const kind = typeof column.column === "string" ? column.column : "value";
  if (kind === "attribute" && typeof column.key === "string") {
    return `Attribute: ${column.key}`;
  }
  if (kind === "token" && typeof column.metric === "string") {
    return `Token: ${column.metric}`;
  }
  return kind.split("_").map((part) => (
    part.length === 0 ? part : `${part[0]!.toUpperCase()}${part.slice(1)}`
  )).join(" ");
}

function ScalarCell({ value }: { readonly value: TaggedScalar | null }): JSX.Element {
  if (value === null) {
    return <span class="table-view__unknown">Unknown</span>;
  }
  switch (value.type) {
    case "null":
      return <span>Null</span>;
    case "boolean":
      return <span>{value.value ? "True" : "False"}</span>;
    case "integer":
    case "decimal":
      return <ExactNumber value={value.value} />;
    case "string":
      return <span class="table-view__business-text">{value.value}</span>;
  }
}

function Coverage({ coverage }: { readonly coverage: ViewCoverage }): JSX.Element {
  return (
    <section
      class="table-view__coverage"
      aria-label="Table coverage"
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

function Binding({ response }: { readonly response: TableViewResponse }): JSX.Element {
  const { binding } = response;
  return (
    <section class="table-view__binding" aria-label="Table binding">
      <h4>Binding</h4>
      <dl>
        <div>
          <dt>Time</dt>
          <dd>{binding.time_range === "viewport" ? "Viewport" : "Run"}</dd>
        </div>
        <div>
          <dt>Range</dt>
          <dd><ExactNumber value={binding.range_start_ns} /> to <ExactNumber value={binding.range_end_ns} /> ns</dd>
        </div>
        <div>
          <dt>Scope</dt>
          <dd>{binding.scope === "selection" ? "Selection" : "Run"}</dd>
        </div>
        <div>
          <dt>Captured watermark</dt>
          <dd><ExactNumber value={binding.captured_watermark} /></dd>
        </div>
      </dl>
    </section>
  );
}

function readyState(response: TableViewResponse): ReadyState {
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
  return response.rows.length === 0 ? "empty" : "ready";
}

function Notices({ response }: { readonly response: TableViewResponse }): JSX.Element {
  const incompatible = response.incompatible;
  if (incompatible !== null) {
    return incompatible.reason === "newer_view_schema" ? (
      <p role="status">
        This table requires view schema {incompatible.record_view_schema_version}; this UI supports
        schema {incompatible.supported_view_schema_version}.
      </p>
    ) : (
      <p role="alert">
        The stored table view record is corrupt. Record schema: {incompatible.record_view_schema_version
          ?? <span class="table-view__unknown">Unknown</span>}.
      </p>
    );
  }
  return (
    <div class="table-view__notices" aria-label="Table result state">
      {response.coverage.status === "partial" ? <p role="status">Coverage is partial.</p> : null}
      {response.coverage.status === "unavailable" ? <p role="status">Coverage is unavailable.</p> : null}
      {response.coverage.gap_count === "0" ? null : (
        <p role="status">Observation gaps affect this table: <ExactNumber value={response.coverage.gap_count} />.</p>
      )}
      {response.truncated ? (
        <p role="status">
          The table is truncated; excluded rows: <ExactNumber
            value={response.coverage.excluded.resource_truncated}
          />.
        </p>
      ) : null}
    </div>
  );
}

function Rows({ response }: { readonly response: TableViewResponse }): JSX.Element {
  if (response.incompatible !== null) {
    return <p class="table-view__unavailable">Table rows are unavailable.</p>;
  }
  if (response.rows.length === 0) {
    return <p class="table-view__empty">No rows in the captured result.</p>;
  }
  return (
    <div class="table-view__rows" style={{ overflowX: "auto", maxWidth: "100%" }}>
      <table>
        <caption>View rows</caption>
        <thead>
          <tr>
            {response.columns.map((column, index) => (
              <th key={index} scope="col">{columnLabel(column)}</th>
            ))}
          </tr>
        </thead>
        <tbody>
          {response.rows.map((row) => (
            <tr key={row.sequence} data-sequence={row.sequence}>
              {row.cells.map((cell, index) => (
                <td
                  key={index}
                  style={{ letterSpacing: 0, maxWidth: "32rem", overflowWrap: "anywhere", wordBreak: "break-word" }}
                >
                  <ScalarCell value={cell} />
                </td>
              ))}
            </tr>
          ))}
        </tbody>
      </table>
    </div>
  );
}

function Result({ response }: { readonly response: TableViewResponse }): JSX.Element {
  return (
    <div
      class="table-view__result"
      data-state={readyState(response)}
      data-gap={response.coverage.gap_count === "0" ? "false" : "true"}
      data-truncated={response.truncated ? "true" : "false"}
    >
      <Notices response={response} />
      <div class="table-view__metadata">
        <Binding response={response} />
        <Coverage coverage={response.coverage} />
      </div>
      <Rows response={response} />
      <footer class="table-view__pagination" aria-label="Table page state">
        <span>Rows on page: {response.rows.length} / {response.pagination?.page_size ?? 0}</span>
        <span>{response.pagination?.next_cursor === null ? "Final page" : "More rows available"}</span>
      </footer>
    </div>
  );
}

export function TableView({ record, state }: TableViewProps): JSX.Element {
  const headingId = `table-view-${record.id}-title`;
  return (
    <section
      class="table-view"
      aria-labelledby={headingId}
      aria-busy={state.status === "loading"}
      data-state={state.status === "ready" ? readyState(state.response) : state.status}
      data-time-binding={state.status === "ready" ? state.response.binding.time_range : record.time_range}
      data-scope-binding={state.status === "ready" ? state.response.binding.scope : record.scope}
      style={{ letterSpacing: 0, minWidth: 0, overflowWrap: "anywhere", wordBreak: "break-word" }}
    >
      <header class="table-view__header">
        <div><p>Table view</p><h3 id={headingId}>{record.title}</h3></div>
        <code>{record.id}</code>
      </header>
      {state.status === "loading" ? (
        <p role="status">Loading table view.</p>
      ) : state.status === "local_error" ? (
        <div class="table-view__local-error" role="alert">
          <strong>Table view failed</strong>
          <code>{state.error.code}</code>
          <p>{state.error.message}</p>
        </div>
      ) : <Result response={state.response} />}
    </section>
  );
}
