import { readFileSync } from "node:fs";
import { resolve } from "node:path";

import { cleanup, render, screen, within } from "@testing-library/preact";
import "@testing-library/jest-dom/vitest";
import { afterEach, describe, expect, it } from "vitest";

import { decodeU64 } from "../../src/protocol/decimal.ts";
import {
  type MetricViewRecord,
  type MetricViewResponse,
  type ViewCoverage,
  decodeViewRecord,
  decodeViewResponse,
} from "../../src/protocol/view.ts";
import { MetricView, type MetricViewState } from "../../src/views/metric.tsx";


const HTTP_FIXTURE = JSON.parse(readFileSync(
  resolve(process.cwd(), "../../tests/fixtures/diagnostics/http/view-metric-v1.json"),
  "utf8",
)) as unknown;
const VIEW_FIXTURE = JSON.parse(readFileSync(
  resolve(process.cwd(), "../../tests/fixtures/diagnostics/views/metric.json"),
  "utf8",
)) as { readonly descriptor: unknown; readonly response: unknown };

function metricRecord(raw: unknown): MetricViewRecord {
  const record = decodeViewRecord(raw);
  if (record.renderer !== "metric") {
    throw new Error("fixture did not decode as a metric record");
  }
  return record;
}

function metricResponse(raw: unknown, record: MetricViewRecord): MetricViewResponse {
  const response = decodeViewResponse(raw, record);
  if (response.renderer !== "metric") {
    throw new Error("fixture did not decode as a metric response");
  }
  return response;
}

const RECORD = metricRecord({
  renderer: "metric",
  view_schema_version: 1,
  id: "metric_view",
  title: "Active turns",
  time_range: "run",
  scope: "run",
  query: {
    source: {
      source: "counter_value",
      selector: { selector: "built_in", kind: "agent.turn.active" },
      selection: "latest_before_reduce",
    },
    filters: [],
    group_by: null,
    reducer: "sum",
  },
});
const RESPONSE = metricResponse(HTTP_FIXTURE, RECORD);

afterEach(cleanup);

function u64(value: string) {
  return decodeU64(value);
}

function coverage({
  status,
  matched,
  contributing,
  excluded = "0",
  missing = "0",
  unavailable = "0",
  truncated = "0",
  gaps = "0",
}: {
  readonly status: ViewCoverage["status"];
  readonly matched: string;
  readonly contributing: string;
  readonly excluded?: string;
  readonly missing?: string;
  readonly unavailable?: string;
  readonly truncated?: string;
  readonly gaps?: string;
}): ViewCoverage {
  return {
    status,
    matched_count: u64(matched),
    contributing_count: u64(contributing),
    excluded_count: u64(excluded),
    excluded: {
      open_spans: u64("0"),
      missing_values: u64(missing),
      non_numeric_values: u64("0"),
      unavailable_values: u64(unavailable),
      resource_truncated: u64(truncated),
    },
    gap_count: u64(gaps),
  };
}

function renderState(state: MetricViewState, record = RECORD) {
  return render(<MetricView record={record} state={state} />);
}

function resultRoot(container: Element): HTMLElement {
  const result = container.querySelector<HTMLElement>(".metric-view__result");
  if (result === null) {
    throw new Error("metric result root is absent");
  }
  return result;
}

describe("C05 metric ViewSpec renderer", () => {
  it("renders the decoded exact value, unit, binding, and coverage without numeric coercion", () => {
    const { container } = renderState({ status: "ready", response: RESPONSE });
    expect(screen.getByRole("heading", { name: "Active turns" })).toBeInTheDocument();
    const series = screen.getByTestId("metric-series-0");
    expect(within(series).getByText("2")).toBeInTheDocument();
    expect(within(series).getByText("count")).toBeInTheDocument();
    expect(within(series).getByLabelText("Metric series 1 coverage"))
      .toHaveTextContent("Matched2Contributing2");
    expect(screen.getByLabelText("Metric binding")).toHaveTextContent("Range0 to 4 ns");
    expect(screen.getByLabelText("Metric coverage")).toHaveTextContent("Observation gaps0");
    expect(resultRoot(container)).toHaveAttribute("data-state", "ready");
  });

  it("shows a mean as its exact numerator and contributing count rather than dividing", () => {
    const meanRecord = metricRecord(VIEW_FIXTURE.descriptor);
    const mean = metricResponse(VIEW_FIXTURE.response, meanRecord);
    const { container } = renderState({ status: "ready", response: mean }, meanRecord);
    const series = screen.getByTestId("metric-series-0");
    expect(series).toHaveTextContent("123456789012345678901234567890 / 3");
    expect(series).toHaveTextContent("tokens");
    expect(series).toHaveTextContent("Exact numerator / contributing count");
    expect(container.textContent).not.toContain("4.1152263004115226e+");
  });

  it("keeps same-group custom units separate and renders business text inertly", () => {
    const payload = "<img src=x>";
    const record = metricRecord({
      ...RECORD,
      id: "custom_metric",
      title: "Custom queue units",
      query: {
        source: {
          source: "counter_value",
          selector: { selector: "custom", name: "example.queue" },
          selection: "latest_before_reduce",
        },
        filters: [],
        group_by: null,
        reducer: "sum",
      },
    });
    const raw = structuredClone(HTTP_FIXTURE) as Record<string, unknown> & {
      series: Array<Record<string, unknown>>;
    };
    raw.view_id = "custom_metric";
    const first = { ...raw.series[0]!, unit: "items" };
    raw.series = [first, { ...structuredClone(first), unit: payload }];
    const response = metricResponse(raw, record);
    const { container } = renderState({ status: "ready", response }, record);
    expect(screen.getAllByText("All facts")).toHaveLength(2);
    expect(screen.getByText("items")).toBeInTheDocument();
    expect(screen.getByText(payload)).toBeInTheDocument();
    expect(container.querySelector("img, script, a")).toBeNull();
  });

  it("distinguishes loading, empty, unavailable, partial, gap, and truncation", () => {
    const view = renderState({ status: "loading" });
    expect(view.container.querySelector(".metric-view")).toHaveAttribute("data-state", "loading");
    expect(screen.getByRole("status")).toHaveTextContent("Loading metric view");

    const cases: readonly [string, MetricViewResponse][] = [
      ["empty", { ...RESPONSE, coverage: coverage({ status: "complete", matched: "0", contributing: "0" }), series: [] }],
      ["unavailable", { ...RESPONSE, coverage: coverage({ status: "unavailable", matched: "1", contributing: "0", excluded: "1", unavailable: "1" }), series: [{ group: null, unit: "count", value: null, coverage: coverage({ status: "unavailable", matched: "1", contributing: "0", excluded: "1", unavailable: "1" }) }] }],
      ["partial", { ...RESPONSE, coverage: coverage({ status: "partial", matched: "1", contributing: "0", excluded: "1", missing: "1" }), series: [] }],
      ["gap", { ...RESPONSE, coverage: coverage({ status: "partial", matched: "0", contributing: "0", gaps: "18446744073709551615" }), series: [] }],
      ["truncated", { ...RESPONSE, coverage: coverage({ status: "partial", matched: "1", contributing: "0", excluded: "1", truncated: "1" }), series: [], truncated: true }],
    ];
    for (const [state, response] of cases) {
      view.rerender(<MetricView record={RECORD} state={{ status: "ready", response }} />);
      expect(resultRoot(view.container)).toHaveAttribute("data-state", state);
    }
    expect(screen.getByText(/metric result is truncated/)).toHaveTextContent("1");
  });

  it("renders newer-schema, corrupt-record, and local errors as separate plain-text states", () => {
    const base: MetricViewResponse = {
      ...RESPONSE,
      coverage: coverage({ status: "unavailable", matched: "0", contributing: "0" }),
      series: [],
      incompatible: {
        reason: "newer_view_schema",
        supported_view_schema_version: 1,
        record_view_schema_version: 2,
      },
    };
    const view = renderState({ status: "ready", response: base });
    expect(resultRoot(view.container)).toHaveAttribute("data-state", "incompatible");
    expect(screen.getByText(/requires view schema 2/)).toHaveTextContent("supports schema 1");
    expect(screen.getByText("Metric series are unavailable.")).toBeInTheDocument();

    view.rerender(<MetricView record={RECORD} state={{
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
    expect(screen.getByRole("alert")).toHaveTextContent("stored metric view record is corrupt");

    const payload = "<script>globalThis.compromised=true</script>";
    view.rerender(<MetricView record={RECORD} state={{
      status: "local_error",
      error: { code: "query_failed", message: payload },
    }} />);
    expect(view.container.querySelector(".metric-view")).toHaveAttribute("data-state", "local_error");
    expect(screen.getByRole("alert")).toHaveTextContent(payload);
    expect(view.container.querySelector("script, img, a")).toBeNull();
  });

  it("has no query transport, aggregate calculation, unsafe markup, or sibling renderer coupling", () => {
    const source = readFileSync(resolve(process.cwd(), "src/views/metric.tsx"), "utf8");
    expect(source).toContain("MetricViewResponse");
    expect(source).not.toMatch(/TimelineViewResponse|TableViewResponse|TimeSeriesViewResponse/);
    expect(source).not.toMatch(/fetch\s*\(|EventSource|XMLHttpRequest|WebSocket/);
    expect(source).not.toMatch(/parseFloat|parseInt|BigInt|\.reduce\s*\(|toFixed|Math\./);
    expect(source).not.toMatch(/dangerouslySetInnerHTML|innerHTML|insertAdjacentHTML/);
  });
});
