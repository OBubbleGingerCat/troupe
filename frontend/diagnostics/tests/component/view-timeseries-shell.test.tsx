import { readFileSync } from "node:fs";
import { resolve } from "node:path";

import { cleanup, render, screen } from "@testing-library/preact";
import "@testing-library/jest-dom/vitest";
import { afterEach, describe, expect, it } from "vitest";

import { decodeU64 } from "../../src/protocol/decimal.ts";
import {
  type TimeSeriesViewRecord,
  type TimeSeriesViewResponse,
  type ViewCoverage,
  decodeViewRecord,
  decodeViewResponse,
} from "../../src/protocol/view.ts";
import {
  TimeSeriesShell,
  selectTimeSeriesMount,
} from "../../src/views/timeseries_shell.tsx";


const HTTP_FIXTURE = JSON.parse(readFileSync(
  resolve(process.cwd(), "../../tests/fixtures/diagnostics/http/view-timeseries-v1.json"),
  "utf8",
)) as unknown;

function timeSeriesRecord(raw: unknown): TimeSeriesViewRecord {
  const record = decodeViewRecord(raw);
  if (record.renderer !== "time_series") {
    throw new Error("fixture did not decode as a time series record");
  }
  return record;
}

function timeSeriesResponse(raw: unknown, record: TimeSeriesViewRecord): TimeSeriesViewResponse {
  const response = decodeViewResponse(raw, record);
  if (response.renderer !== "time_series") {
    throw new Error("fixture did not decode as a time series response");
  }
  return response;
}

const RECORD = timeSeriesRecord({
  renderer: "time_series",
  view_schema_version: 1,
  id: "timeseries_view",
  title: "Cue admissions over time",
  time_range: "run",
  scope: "run",
  query: {
    source: {
      source: "instant_count",
      selector: { selector: "built_in", kind: "cue.admitted" },
    },
    filters: [],
    group_by: null,
    reducer: "count",
  },
});
const RESPONSE = timeSeriesResponse(HTTP_FIXTURE, RECORD);

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

function resultRoot(container: Element): HTMLElement {
  const result = container.querySelector<HTMLElement>(".timeseries-shell__result");
  if (result === null) {
    throw new Error("time series result root is absent");
  }
  return result;
}

describe("C05 time series ViewSpec shell", () => {
  it("exposes the server-aligned response as exact bounded columns", () => {
    const model = selectTimeSeriesMount(RESPONSE);
    expect(model).not.toBeNull();
    expect(model?.range_start_ns).toBe("0");
    expect(model?.range_end_ns).toBe("4");
    expect(model?.bucket_width_ns).toBe("1");
    expect(model?.series).toHaveLength(1);
    expect(model?.series[0]?.bucket_start_ns).toEqual(["0", "1", "2", "3"]);
    expect(model?.series[0]?.bucket_end_ns).toEqual(["1", "2", "3", "4"]);
    expect(model?.series[0]?.values[1]).toEqual({
      aggregate: "exact",
      value: { type: "integer", value: "1" },
    });

    const { container } = render(<TimeSeriesShell record={RECORD} state={{ status: "ready", response: RESPONSE }} />);
    const mount = screen.getByRole("group", { name: "Time series plot" });
    expect(mount).toHaveAttribute("data-series-count", "1");
    expect(mount).toHaveAttribute("data-point-count", "4");
    expect(mount).toHaveAttribute("data-bucket-width-ns", "1");
    expect(screen.getByLabelText("Time series binding")).toHaveTextContent("Range0 to 4 ns");
    expect(screen.getByLabelText("Time series coverage")).toHaveTextContent("Contributing2");
    expect(container.querySelector("canvas")).toBeNull();
  });

  it("distinguishes loading and an aligned empty range", () => {
    const view = render(<TimeSeriesShell record={RECORD} state={{ status: "loading" }} />);
    expect(view.container.querySelector(".timeseries-shell")).toHaveAttribute("data-state", "loading");
    expect(screen.getByRole("status")).toHaveTextContent("Loading time series view");

    const empty: TimeSeriesViewResponse = {
      ...RESPONSE,
      binding: {
        ...RESPONSE.binding,
        captured_elapsed_end_ns: u64("0"),
        range_end_ns: u64("0"),
      },
      coverage: coverage({ status: "complete", matched: "0", contributing: "0" }),
      series: [],
    };
    view.rerender(<TimeSeriesShell record={RECORD} state={{ status: "ready", response: empty }} />);
    expect(resultRoot(view.container)).toHaveAttribute("data-state", "empty");
    expect(screen.getByText("No time series points in the captured range.")).toBeInTheDocument();
  });

  it("keeps partial, gap, truncation, and unavailable states distinct", () => {
    const partialCoverage = coverage({
      status: "partial",
      matched: "2",
      contributing: "1",
      excluded: "1",
      missingValues: "1",
    });
    const view = render(<TimeSeriesShell record={RECORD} state={{
      status: "ready",
      response: { ...RESPONSE, coverage: partialCoverage },
    }} />);
    expect(resultRoot(view.container)).toHaveAttribute("data-state", "partial");

    view.rerender(<TimeSeriesShell record={RECORD} state={{
      status: "ready",
      response: { ...RESPONSE, coverage: coverage({ status: "partial", matched: "2", contributing: "2", gaps: "18446744073709551615" }) },
    }} />);
    expect(resultRoot(view.container)).toHaveAttribute("data-state", "gap");
    expect(screen.getByText(/Observation gaps affect/)).toHaveTextContent("18446744073709551615");

    view.rerender(<TimeSeriesShell record={RECORD} state={{
      status: "ready",
      response: {
        ...RESPONSE,
        coverage: coverage({ status: "partial", matched: "1", contributing: "0", excluded: "1", resourceTruncated: "1" }),
        truncated: true,
      },
    }} />);
    expect(resultRoot(view.container)).toHaveAttribute("data-state", "truncated");

    view.rerender(<TimeSeriesShell record={RECORD} state={{
      status: "ready",
      response: { ...RESPONSE, coverage: coverage({ status: "unavailable", matched: "0", contributing: "0" }), series: [] },
    }} />);
    expect(resultRoot(view.container)).toHaveAttribute("data-state", "unavailable");
  });

  it("normalizes typed incompatible and corrupt response states", () => {
    const incompatible: TimeSeriesViewResponse = {
      ...RESPONSE,
      coverage: coverage({ status: "unavailable", matched: "0", contributing: "0" }),
      series: [],
      incompatible: {
        reason: "newer_view_schema",
        supported_view_schema_version: 1,
        record_view_schema_version: 2,
      },
    };
    const view = render(<TimeSeriesShell record={RECORD} state={{ status: "ready", response: incompatible }} />);
    expect(resultRoot(view.container)).toHaveAttribute("data-state", "incompatible");
    expect(selectTimeSeriesMount(incompatible)).toBeNull();
    expect(screen.getByText(/requires view schema 2/)).toHaveTextContent("supports schema 1");

    view.rerender(<TimeSeriesShell record={RECORD} state={{
      status: "ready",
      response: {
        ...incompatible,
        incompatible: {
          reason: "corrupt_record",
          supported_view_schema_version: 1,
          record_view_schema_version: null,
        },
      },
    }} />);
    expect(resultRoot(view.container)).toHaveAttribute("data-state", "corrupt");
    expect(screen.getByRole("alert")).toHaveTextContent("stored time series view record is corrupt");
  });

  it("shows an unsupported archive record as normalized panel unavailability", () => {
    const view = render(<TimeSeriesShell record={null} state={{
      status: "archive_unavailable",
      unavailable: {
        reason: "newer_view_schema",
        supported_view_schema_version: 1,
        record_view_schema_version: 7,
      },
    }} />);
    expect(view.container.querySelector(".timeseries-shell")).toHaveAttribute("data-state", "archive_unavailable");
    expect(screen.getByRole("status")).toHaveTextContent("Archived time series unavailable");
    expect(screen.getByRole("status")).toHaveTextContent("schema 7");
  });

  it("keeps local error strings inert", () => {
    const payload = "<img src=x onerror=globalThis.compromised=true>";
    const { container } = render(<TimeSeriesShell record={RECORD} state={{
      status: "local_error",
      error: { code: "query_failed", message: payload },
    }} />);
    expect(container.querySelector(".timeseries-shell")).toHaveAttribute("data-state", "local_error");
    expect(screen.getByRole("alert")).toHaveTextContent(payload);
    expect(container.querySelector("img, script, a")).toBeNull();
  });

  it("has no uPlot, query, rebucketing, numeric coercion, or custom renderer hook", () => {
    const source = readFileSync(resolve(process.cwd(), "src/views/timeseries_shell.tsx"), "utf8");
    expect(source).toContain("TimeSeriesViewResponse");
    expect(source).not.toMatch(/TimelineViewResponse|MetricViewResponse|TableViewResponse/);
    expect(source).not.toMatch(/uPlot|new\s+uPlot|canvas|getContext|requestAnimationFrame/i);
    expect(source).not.toMatch(/fetch\s*\(|EventSource|XMLHttpRequest|WebSocket/);
    expect(source).not.toMatch(/parseFloat|parseInt|Number\s*\(|Math\.(ceil|floor|round)/);
    expect(source).not.toMatch(/renderPlot|rendererFactory|plugin|dangerouslySetInnerHTML|innerHTML/);
  });
});
