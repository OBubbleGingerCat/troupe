import { readFileSync } from "node:fs";
import { resolve } from "node:path";

import { cleanup, render, screen } from "@testing-library/preact";
import "@testing-library/jest-dom/vitest";
import { afterEach, describe, expect, it } from "vitest";

import { decodeU64 } from "../../src/protocol/decimal.ts";
import {
  type TableViewRecord,
  type TableViewResponse,
  type ViewCoverage,
  decodeViewRecord,
  decodeViewResponse,
} from "../../src/protocol/view.ts";
import { TableView, type TableViewState } from "../../src/views/table.tsx";


const HTTP_FIXTURE = JSON.parse(readFileSync(
  resolve(process.cwd(), "../../tests/fixtures/diagnostics/http/view-table-v1.json"),
  "utf8",
)) as unknown;

function tableRecord(raw: unknown): TableViewRecord {
  const record = decodeViewRecord(raw);
  if (record.renderer !== "table") {
    throw new Error("fixture did not decode as a table record");
  }
  return record;
}

function tableResponse(raw: unknown, record: TableViewRecord): TableViewResponse {
  const response = decodeViewResponse(raw, record);
  if (response.renderer !== "table") {
    throw new Error("fixture did not decode as a table response");
  }
  return response;
}

const RECORD = tableRecord({
  renderer: "table",
  view_schema_version: 1,
  id: "table_view",
  title: "Event facts",
  time_range: "run",
  scope: "run",
  query: {
    source: { source: "event", kind: "instant_occurred" },
    filters: [],
    columns: [{ column: "sequence" }, { column: "elapsed_ns" }],
    page_size: 1,
  },
});
const RESPONSE = tableResponse(HTTP_FIXTURE, RECORD);

afterEach(cleanup);

function u64(value: string) {
  return decodeU64(value);
}

function coverage({
  status,
  matched,
  contributing,
  excluded = "0",
  missingValues = "0",
  resourceTruncated = "0",
  gaps = "0",
}: {
  readonly status: ViewCoverage["status"];
  readonly matched: string;
  readonly contributing: string;
  readonly excluded?: string;
  readonly missingValues?: string;
  readonly resourceTruncated?: string;
  readonly gaps?: string;
}): ViewCoverage {
  return {
    status,
    matched_count: u64(matched),
    contributing_count: u64(contributing),
    excluded_count: u64(excluded),
    excluded: {
      open_spans: u64("0"),
      missing_values: u64(missingValues),
      non_numeric_values: u64("0"),
      unavailable_values: u64("0"),
      resource_truncated: u64(resourceTruncated),
    },
    gap_count: u64(gaps),
  };
}

function renderState(state: TableViewState, record = RECORD) {
  return render(<TableView record={record} state={state} />);
}

function resultRoot(container: Element): HTMLElement {
  const result = container.querySelector<HTMLElement>(".table-view__result");
  if (result === null) {
    throw new Error("table result root is absent");
  }
  return result;
}

describe("C05 table ViewSpec renderer", () => {
  it("renders closed columns and exact cells without exposing the opaque cursor", () => {
    const { container } = renderState({ status: "ready", response: RESPONSE });
    expect(screen.getByRole("heading", { name: "Event facts" })).toBeInTheDocument();
    expect(screen.getByRole("table", { name: "View rows" })).toBeInTheDocument();
    expect(screen.getByRole("columnheader", { name: "Sequence" })).toBeInTheDocument();
    expect(screen.getByRole("columnheader", { name: "Elapsed Ns" })).toBeInTheDocument();
    expect(screen.getAllByRole("cell").map((cell) => cell.textContent)).toEqual(["1", "1"]);
    expect(screen.getByText("More rows available")).toBeInTheDocument();
    expect(container.textContent).not.toContain("q1.");
    expect(screen.getByLabelText("Table binding")).toHaveTextContent("Captured watermark2");
    expect(screen.getByLabelText("Table coverage")).toHaveTextContent("Contributing2");
  });

  it("keeps arbitrary exact values and long business text lossless and inert", () => {
    const payload = `<img src=x onerror=globalThis.compromised=true>${"x".repeat(256)}`;
    const record = tableRecord({
      ...RECORD,
      id: "typed_values",
      query: {
        ...RECORD.query,
        columns: [
          { column: "value" },
          { column: "attribute", key: "label" },
          { column: "outcome" },
        ],
        page_size: 10,
      },
    });
    const response: TableViewResponse = {
      ...RESPONSE,
      view_id: "typed_values",
      columns: record.query.columns as TableViewResponse["columns"],
      pagination: { page_size: 10, next_cursor: null },
      rows: [{
        sequence: u64("1"),
        cells: [
          { type: "integer", value: "123456789012345678901234567890" },
          { type: "string", value: payload },
          null,
        ],
      }],
    };
    const { container } = renderState({ status: "ready", response }, record);
    expect(screen.getByText("123456789012345678901234567890")).toBeInTheDocument();
    expect(screen.getByText(payload)).toBeInTheDocument();
    expect(screen.getByText("Unknown")).toBeInTheDocument();
    expect(screen.getByRole("columnheader", { name: "Attribute: label" })).toBeInTheDocument();
    expect(container.querySelector("img, script, a")).toBeNull();
  });

  it("distinguishes loading, empty, unavailable, partial, gap, and truncation", () => {
    const view = renderState({ status: "loading" });
    expect(view.container.querySelector(".table-view")).toHaveAttribute("data-state", "loading");
    expect(screen.getByRole("status")).toHaveTextContent("Loading table view");

    const cases: readonly [string, TableViewResponse][] = [
      ["empty", { ...RESPONSE, coverage: coverage({ status: "complete", matched: "0", contributing: "0" }), rows: [], pagination: { page_size: 1, next_cursor: null } }],
      ["unavailable", { ...RESPONSE, coverage: coverage({ status: "unavailable", matched: "0", contributing: "0" }), rows: [] }],
      ["partial", { ...RESPONSE, coverage: coverage({ status: "partial", matched: "1", contributing: "0", excluded: "1", missingValues: "1" }), rows: [] }],
      ["gap", { ...RESPONSE, coverage: coverage({ status: "partial", matched: "0", contributing: "0", gaps: "18446744073709551615" }), rows: [] }],
      ["truncated", { ...RESPONSE, coverage: coverage({ status: "partial", matched: "1", contributing: "0", excluded: "1", resourceTruncated: "1" }), rows: [], truncated: true }],
    ];
    for (const [state, response] of cases) {
      view.rerender(<TableView record={RECORD} state={{ status: "ready", response }} />);
      expect(resultRoot(view.container)).toHaveAttribute("data-state", state);
    }
    expect(screen.getByText(/table is truncated/)).toHaveTextContent("1");
  });

  it("normalizes newer-schema and corrupt archive records separately", () => {
    const base: TableViewResponse = {
      ...RESPONSE,
      coverage: coverage({ status: "unavailable", matched: "0", contributing: "0" }),
      rows: [],
      pagination: { page_size: 1, next_cursor: null },
      incompatible: {
        reason: "newer_view_schema",
        supported_view_schema_version: 1,
        record_view_schema_version: 2,
      },
    };
    const view = renderState({ status: "ready", response: base });
    expect(resultRoot(view.container)).toHaveAttribute("data-state", "incompatible");
    expect(screen.getByText(/requires view schema 2/)).toHaveTextContent("supports schema 1");
    expect(screen.getByText("Table rows are unavailable.")).toBeInTheDocument();

    view.rerender(<TableView record={RECORD} state={{
      status: "ready",
      response: {
        ...base,
        incompatible: {
          reason: "corrupt_record",
          supported_view_schema_version: 1,
          record_view_schema_version: null,
        },
      },
    }} />);
    expect(resultRoot(view.container)).toHaveAttribute("data-state", "corrupt");
    expect(screen.getByRole("alert")).toHaveTextContent("stored table view record is corrupt");
  });

  it("renders local error strings as plain text", () => {
    const payload = "<script>globalThis.compromised=true</script>";
    const { container } = renderState({
      status: "local_error",
      error: { code: "query_failed", message: payload },
    });
    expect(container.querySelector(".table-view")).toHaveAttribute("data-state", "local_error");
    expect(screen.getByRole("alert")).toHaveTextContent(payload);
    expect(container.querySelector("script, img, a")).toBeNull();
  });

  it("has no fetch, cursor parsing, unsafe markup, or sibling renderer coupling", () => {
    const source = readFileSync(resolve(process.cwd(), "src/views/table.tsx"), "utf8");
    expect(source).toContain("TableViewResponse");
    expect(source).not.toMatch(/TimelineViewResponse|MetricViewResponse|TimeSeriesViewResponse/);
    expect(source).not.toMatch(/fetch\s*\(|EventSource|XMLHttpRequest|WebSocket/);
    expect(source).not.toMatch(/atob|fromBase64|split\([^)]*cursor|JSON\.parse/);
    expect(source).not.toMatch(/dangerouslySetInnerHTML|innerHTML|insertAdjacentHTML/);
  });
});
