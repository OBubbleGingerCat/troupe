import type {
  ComponentChildren,
  JSX,
} from "preact";

import type { JsonValue } from "../protocol/decimal.ts";
import type { DiagnosticScope } from "../protocol/event.ts";
import type {
  TimelineViewRecord,
  TimelineViewResponse,
  ViewCoverage,
} from "../protocol/view.ts";


export interface TimelineViewLocalError {
  readonly code: string;
  readonly message: string;
}

export type TimelineViewState =
  | { readonly status: "loading" }
  | { readonly status: "ready"; readonly response: TimelineViewResponse }
  | { readonly status: "local_error"; readonly error: TimelineViewLocalError };

export interface TimelineViewProps {
  readonly record: TimelineViewRecord;
  readonly state: TimelineViewState;
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

interface FieldProps {
  readonly label: string;
  readonly children: ComponentChildren;
}

function Field({ label, children }: FieldProps): JSX.Element {
  return (
    <div class="timeline-view__field">
      <dt>{label}</dt>
      <dd>{children}</dd>
    </div>
  );
}

function ExactNumber({ value }: { readonly value: string }): JSX.Element {
  return <code class="timeline-view__number">{value}</code>;
}

function JsonValueText({ value }: { readonly value: JsonValue }): JSX.Element {
  if (value === null) {
    return <span class="timeline-view__unknown">Unknown</span>;
  }
  if (typeof value === "string") {
    return <span>{value}</span>;
  }
  if (typeof value === "number") {
    return <code>{String(value)}</code>;
  }
  if (typeof value === "boolean") {
    return <span>{value ? "True" : "False"}</span>;
  }
  if (Array.isArray(value)) {
    return value.length === 0 ? <span>Empty list</span> : (
      <ol class="timeline-view__json-list">
        {value.map((item, index) => (
          <li key={index}><JsonValueText value={item} /></li>
        ))}
      </ol>
    );
  }
  const entries = Object.entries(value);
  return entries.length === 0 ? <span>Empty object</span> : (
    <dl class="timeline-view__json-object">
      {entries.map(([key, item]) => (
        <Field key={key} label={key}><JsonValueText value={item} /></Field>
      ))}
    </dl>
  );
}

function ScopeFields({ scope }: { readonly scope: DiagnosticScope }): JSX.Element {
  return (
    <dl class="timeline-view__scope">
      <Field label="Scene">{scope.scene_id ?? <span class="timeline-view__unknown">Unknown</span>}</Field>
      <Field label="Actor">{scope.actor_id ?? <span class="timeline-view__unknown">Unknown</span>}</Field>
      <Field label="Cue">{scope.cue_id ?? <span class="timeline-view__unknown">Unknown</span>}</Field>
      <Field label="Effect">{scope.effect_id ?? <span class="timeline-view__unknown">Unknown</span>}</Field>
      <Field label="Act">{scope.act_id ?? <span class="timeline-view__unknown">Unknown</span>}</Field>
      <Field label="Tool call">{scope.tool_call_id ?? <span class="timeline-view__unknown">Unknown</span>}</Field>
      <Field label="Session generation">
        {scope.session_generation === null
          ? <span class="timeline-view__unknown">Unknown</span>
          : <ExactNumber value={scope.session_generation} />}
      </Field>
    </dl>
  );
}

function Binding({ response }: { readonly response: TimelineViewResponse }): JSX.Element {
  const binding = response.binding;
  return (
    <section class="timeline-view__binding" aria-label="Timeline binding">
      <h4>Binding</h4>
      <dl>
        <Field label="Time">
          <span>{binding.time_range === "viewport" ? "Viewport" : "Run"}</span>
        </Field>
        <Field label="Range">
          <ExactNumber value={binding.range_start_ns} />
          <span> to </span>
          <ExactNumber value={binding.range_end_ns} />
          <span> ns</span>
        </Field>
        <Field label="Scope">
          <span>{binding.scope === "selection" ? "Selection" : "Run"}</span>
        </Field>
        <Field label="Selected scope">
          {binding.selected_scope === null
            ? <span>{binding.scope === "selection" ? "Run fallback" : "Run"}</span>
            : <ScopeFields scope={binding.selected_scope} />}
        </Field>
        <Field label="Captured watermark">
          <ExactNumber value={binding.captured_watermark} />
        </Field>
        <Field label="Captured elapsed end">
          <ExactNumber value={binding.captured_elapsed_end_ns} />
          <span> ns</span>
        </Field>
      </dl>
    </section>
  );
}

function Coverage({ coverage }: { readonly coverage: ViewCoverage }): JSX.Element {
  return (
    <section
      class="timeline-view__coverage"
      aria-label="Timeline coverage"
      data-coverage={coverage.status}
    >
      <h4>Coverage</h4>
      <dl>
        <Field label="Status">{coverage.status}</Field>
        <Field label="Matched"><ExactNumber value={coverage.matched_count} /></Field>
        <Field label="Contributing"><ExactNumber value={coverage.contributing_count} /></Field>
        <Field label="Excluded"><ExactNumber value={coverage.excluded_count} /></Field>
        <Field label="Open spans"><ExactNumber value={coverage.excluded.open_spans} /></Field>
        <Field label="Missing values"><ExactNumber value={coverage.excluded.missing_values} /></Field>
        <Field label="Non-numeric values">
          <ExactNumber value={coverage.excluded.non_numeric_values} />
        </Field>
        <Field label="Unavailable values">
          <ExactNumber value={coverage.excluded.unavailable_values} />
        </Field>
        <Field label="Resource truncated">
          <ExactNumber value={coverage.excluded.resource_truncated} />
        </Field>
        <Field label="Observation gaps"><ExactNumber value={coverage.gap_count} /></Field>
      </dl>
    </section>
  );
}

function readyState(response: TimelineViewResponse): ReadyState {
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

function StateNotices({ response }: { readonly response: TimelineViewResponse }): JSX.Element {
  const incompatible = response.incompatible;
  if (incompatible !== null) {
    return (
      <div class="timeline-view__notices">
        {incompatible.reason === "newer_view_schema" ? (
          <p role="status">
            This timeline requires view schema {incompatible.record_view_schema_version}; this UI
            supports schema {incompatible.supported_view_schema_version}.
          </p>
        ) : (
          <p role="alert">
            The stored timeline view record is corrupt. Record schema: {incompatible.record_view_schema_version
              ?? <span class="timeline-view__unknown">Unknown</span>}.
          </p>
        )}
      </div>
    );
  }
  return (
    <div class="timeline-view__notices" aria-label="Timeline result state">
      {response.coverage.status === "partial" ? <p role="status">Coverage is partial.</p> : null}
      {response.coverage.status === "unavailable"
        ? <p role="status">Coverage is unavailable.</p>
        : null}
      {response.coverage.gap_count !== "0" ? (
        <p role="status">
          Observation gaps affect this result: <ExactNumber value={response.coverage.gap_count} />.
        </p>
      ) : null}
      {response.truncated ? (
        <p role="status">
          The result is truncated; excluded rows: <ExactNumber
            value={response.coverage.excluded.resource_truncated}
          />.
        </p>
      ) : null}
    </div>
  );
}

function RowTime({
  row,
  capturedEnd,
}: {
  readonly row: TimelineViewResponse["rows"][number];
  readonly capturedEnd: string;
}): JSX.Element {
  if (row.item_type === "instant") {
    return <span>At <ExactNumber value={row.start_ns} /> ns</span>;
  }
  return (
    <span>
      <ExactNumber value={row.start_ns} />
      <span> to </span>
      {row.end_ns === null ? (
        <><ExactNumber value={capturedEnd} /><span> ns (open)</span></>
      ) : (
        <><ExactNumber value={row.end_ns} /><span> ns</span></>
      )}
    </span>
  );
}

function TimelineRows({ response }: { readonly response: TimelineViewResponse }): JSX.Element {
  if (response.incompatible !== null) {
    return <p class="timeline-view__unavailable">Timeline rows are unavailable.</p>;
  }
  if (response.rows.length === 0) {
    return <p class="timeline-view__empty">No timeline rows in the captured result.</p>;
  }
  return (
    <div class="timeline-view__rows">
      <table>
        <caption>Timeline rows</caption>
        <thead>
          <tr>
            <th scope="col">Sequence</th>
            <th scope="col">Group</th>
            <th scope="col">Item</th>
            <th scope="col">Name</th>
            <th scope="col">Time</th>
            <th scope="col">Scope</th>
            <th scope="col">Outcome</th>
          </tr>
        </thead>
        <tbody>
          {response.rows.map((row) => (
            <tr key={row.sequence} data-item-type={row.item_type}>
              <td><ExactNumber value={row.sequence} /></td>
              <td>{row.group === null ? <span>All</span> : <JsonValueText value={row.group} />}</td>
              <td>{row.item_type}</td>
              <td class="timeline-view__business-text">{row.name}</td>
              <td>
                <RowTime row={row} capturedEnd={response.binding.captured_elapsed_end_ns} />
              </td>
              <td><ScopeFields scope={row.scope} /></td>
              <td>
                {row.item_type === "instant"
                  ? <span>Not applicable</span>
                  : row.outcome ?? <span>Open</span>}
              </td>
            </tr>
          ))}
        </tbody>
      </table>
    </div>
  );
}

function Result({ response }: { readonly response: TimelineViewResponse }): JSX.Element {
  const state = readyState(response);
  return (
    <div
      class="timeline-view__result"
      data-state={state}
      data-empty={response.rows.length === 0 ? "true" : "false"}
      data-gap={response.coverage.gap_count !== "0" ? "true" : "false"}
      data-truncated={response.truncated ? "true" : "false"}
    >
      <StateNotices response={response} />
      <div class="timeline-view__metadata">
        <Binding response={response} />
        <Coverage coverage={response.coverage} />
      </div>
      <TimelineRows response={response} />
      <footer class="timeline-view__pagination" aria-label="Timeline page state">
        {response.pagination === null ? <span>Pagination unavailable</span> : (
          <>
            <span>Rows on page: {response.rows.length} / {response.pagination.page_size}</span>
            <span>{response.pagination.next_cursor === null
              ? "Final page"
              : "More rows available"}</span>
          </>
        )}
      </footer>
    </div>
  );
}

export function TimelineView({ record, state }: TimelineViewProps): JSX.Element {
  const headingId = `timeline-view-${record.id}-title`;
  return (
    <section
      class="timeline-view"
      aria-labelledby={headingId}
      aria-busy={state.status === "loading"}
      data-state={state.status === "ready" ? readyState(state.response) : state.status}
      data-time-binding={state.status === "ready"
        ? state.response.binding.time_range
        : record.time_range}
      data-scope-binding={state.status === "ready"
        ? state.response.binding.scope
        : record.scope}
      style={{
        letterSpacing: 0,
        minWidth: 0,
        overflowWrap: "anywhere",
        wordBreak: "break-word",
      }}
    >
      <header class="timeline-view__header">
        <div>
          <p>Timeline view</p>
          <h3 id={headingId} class="timeline-view__business-text">{record.title}</h3>
        </div>
        <code>{record.id}</code>
      </header>

      {state.status === "loading" ? (
        <p class="timeline-view__loading" role="status">Loading timeline view.</p>
      ) : state.status === "local_error" ? (
        <div class="timeline-view__local-error" role="alert">
          <strong>Timeline view failed</strong>
          <code>{state.error.code}</code>
          <p class="timeline-view__business-text">{state.error.message}</p>
        </div>
      ) : <Result response={state.response} />}
    </section>
  );
}
