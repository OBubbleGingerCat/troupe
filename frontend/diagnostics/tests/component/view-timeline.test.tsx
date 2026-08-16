import { readFileSync } from "node:fs";
import { resolve } from "node:path";

import {
  cleanup,
  render,
  screen,
  within,
} from "@testing-library/preact";
import "@testing-library/jest-dom/vitest";
import {
  afterEach,
  describe,
  expect,
  it,
} from "vitest";

import { decodeU64 } from "../../src/protocol/decimal.ts";
import type { DiagnosticScope } from "../../src/protocol/event.ts";
import {
  type TimelineViewRecord,
  type TimelineViewResponse,
  type ViewCoverage,
  decodeViewRecord,
  decodeViewResponse,
} from "../../src/protocol/view.ts";
import {
  TimelineView,
  type TimelineViewState,
} from "../../src/views/timeline.tsx";


const HTTP_FIXTURE = JSON.parse(readFileSync(
  resolve(process.cwd(), "../../tests/fixtures/diagnostics/http/view-timeline-v1.json"),
  "utf8",
)) as unknown;

function timelineRecord(raw: unknown): TimelineViewRecord {
  const record = decodeViewRecord(raw);
  if (record.renderer !== "timeline") {
    throw new Error("fixture did not decode as a timeline record");
  }
  return record;
}

function timelineResponse(raw: unknown, record: TimelineViewRecord): TimelineViewResponse {
  const response = decodeViewResponse(raw, record);
  if (response.renderer !== "timeline") {
    throw new Error("fixture did not decode as a timeline response");
  }
  return response;
}

const RECORD = timelineRecord({
  renderer: "timeline",
  view_schema_version: 1,
  id: "timeline_view",
  title: "Cue admission timeline",
  time_range: "run",
  scope: "run",
  query: {
    source: {
      source: "instant",
      selector: { selector: "built_in", kind: "cue.admitted" },
    },
    filters: [],
    group_by: null,
  },
});
const RESPONSE = timelineResponse(HTTP_FIXTURE, RECORD);

afterEach(cleanup);

function u64(value: string) {
  return decodeU64(value);
}

function coverage({
  status,
  matched,
  contributing,
  excluded = "0",
  openSpans = "0",
  missingValues = "0",
  nonNumericValues = "0",
  unavailableValues = "0",
  resourceTruncated = "0",
  gaps = "0",
}: {
  readonly status: ViewCoverage["status"];
  readonly matched: string;
  readonly contributing: string;
  readonly excluded?: string;
  readonly openSpans?: string;
  readonly missingValues?: string;
  readonly nonNumericValues?: string;
  readonly unavailableValues?: string;
  readonly resourceTruncated?: string;
  readonly gaps?: string;
}): ViewCoverage {
  return {
    status,
    matched_count: u64(matched),
    contributing_count: u64(contributing),
    excluded_count: u64(excluded),
    excluded: {
      open_spans: u64(openSpans),
      missing_values: u64(missingValues),
      non_numeric_values: u64(nonNumericValues),
      unavailable_values: u64(unavailableValues),
      resource_truncated: u64(resourceTruncated),
    },
    gap_count: u64(gaps),
  };
}

function renderState(state: TimelineViewState, record = RECORD) {
  return render(<TimelineView record={record} state={state} />);
}

function resultRoot(container: Element): HTMLElement {
  const result = container.querySelector<HTMLElement>(".timeline-view__result");
  if (result === null) {
    throw new Error("timeline result root is absent");
  }
  return result;
}

describe("C05 timeline ViewSpec renderer", () => {
  it("renders only the typed timeline page with exact run binding, coverage, and cursor presence", () => {
    const { container } = renderState({ status: "ready", response: RESPONSE });
    const panel = container.querySelector(".timeline-view");

    expect(panel).toHaveAttribute("data-state", "ready");
    expect(panel).toHaveAttribute("data-time-binding", "run");
    expect(panel).toHaveAttribute("data-scope-binding", "run");
    expect(screen.getByRole("heading", { name: "Cue admission timeline" })).toBeInTheDocument();
    expect(screen.getByRole("table", { name: "Timeline rows" })).toBeInTheDocument();
    expect(screen.getByText("cue.admitted")).toBeInTheDocument();
    expect(screen.getByText("More rows available")).toBeInTheDocument();
    expect(container.textContent).not.toContain("q1.");

    const binding = screen.getByLabelText("Timeline binding");
    expect(binding).toHaveTextContent("Range0 to 4 ns");
    expect(binding).toHaveTextContent("Captured watermark2");
    const resultCoverage = screen.getByLabelText("Timeline coverage");
    expect(resultCoverage).toHaveAttribute("data-coverage", "complete");
    expect(resultCoverage).toHaveTextContent("Matched2");
    expect(resultCoverage).toHaveTextContent("Contributing2");
    expect(resultCoverage).toHaveTextContent("Observation gaps0");
  });

  it("distinguishes controlled loading and protocol-valid empty states", () => {
    const view = renderState({ status: "loading" });
    const panel = view.container.querySelector(".timeline-view");
    expect(panel).toHaveAttribute("aria-busy", "true");
    expect(panel).toHaveAttribute("data-state", "loading");
    expect(screen.getByRole("status")).toHaveTextContent("Loading timeline view.");

    const empty: TimelineViewResponse = {
      ...RESPONSE,
      coverage: coverage({ status: "complete", matched: "0", contributing: "0" }),
      pagination: { page_size: 100, next_cursor: null },
      rows: [],
    };
    view.rerender(<TimelineView record={RECORD} state={{ status: "ready", response: empty }} />);
    expect(resultRoot(view.container)).toHaveAttribute("data-state", "empty");
    expect(screen.getByText("No timeline rows in the captured result.")).toBeInTheDocument();
    expect(screen.getByText("Final page")).toBeInTheDocument();
  });

  it("keeps partial, gap, and truncation evidence distinct and exact", () => {
    const partial: TimelineViewResponse = {
      ...RESPONSE,
      coverage: coverage({
        status: "partial",
        matched: "2",
        contributing: "1",
        excluded: "1",
        missingValues: "1",
      }),
    };
    const view = renderState({ status: "ready", response: partial });
    expect(resultRoot(view.container)).toHaveAttribute("data-state", "partial");
    expect(screen.getByText("Coverage is partial.")).toBeInTheDocument();
    expect(screen.getByLabelText("Timeline coverage")).toHaveTextContent("Missing values1");

    const gap: TimelineViewResponse = {
      ...RESPONSE,
      coverage: coverage({
        status: "partial",
        matched: "2",
        contributing: "2",
        gaps: "18446744073709551615",
      }),
    };
    view.rerender(<TimelineView record={RECORD} state={{ status: "ready", response: gap }} />);
    expect(resultRoot(view.container)).toHaveAttribute("data-state", "gap");
    expect(resultRoot(view.container)).toHaveAttribute("data-gap", "true");
    expect(screen.getByText(/Observation gaps affect this result/)).toHaveTextContent(
      "18446744073709551615",
    );

    const truncated: TimelineViewResponse = {
      ...RESPONSE,
      coverage: coverage({
        status: "partial",
        matched: "2",
        contributing: "1",
        excluded: "1",
        resourceTruncated: "1",
      }),
      truncated: true,
    };
    view.rerender(
      <TimelineView record={RECORD} state={{ status: "ready", response: truncated }} />,
    );
    expect(resultRoot(view.container)).toHaveAttribute("data-state", "truncated");
    expect(resultRoot(view.container)).toHaveAttribute("data-truncated", "true");
    expect(screen.getByText(/The result is truncated/)).toHaveTextContent("1");
  });

  it("renders newer-schema incompatible and corrupt-record states separately", () => {
    const unavailable = coverage({ status: "unavailable", matched: "0", contributing: "0" });
    const incompatible: TimelineViewResponse = {
      ...RESPONSE,
      coverage: unavailable,
      pagination: { page_size: 100, next_cursor: null },
      rows: [],
      incompatible: {
        reason: "newer_view_schema",
        supported_view_schema_version: 1,
        record_view_schema_version: 2,
      },
    };
    const view = renderState({ status: "ready", response: incompatible });
    expect(resultRoot(view.container)).toHaveAttribute("data-state", "incompatible");
    expect(screen.getByText(/requires view schema 2/)).toHaveTextContent("supports schema 1");
    expect(screen.getByText("Timeline rows are unavailable.")).toBeInTheDocument();

    const corrupt: TimelineViewResponse = {
      ...incompatible,
      incompatible: {
        reason: "corrupt_record",
        supported_view_schema_version: 1,
        record_view_schema_version: null,
      },
    };
    view.rerender(<TimelineView record={RECORD} state={{ status: "ready", response: corrupt }} />);
    expect(resultRoot(view.container)).toHaveAttribute("data-state", "corrupt");
    expect(screen.getByRole("alert")).toHaveTextContent("stored timeline view record is corrupt");
    expect(screen.getByRole("alert")).toHaveTextContent("Unknown");
  });

  it("renders viewport and selected-scope binding without changing decimal identities", () => {
    const selectedScope: DiagnosticScope = {
      scene_id: "scene-1",
      actor_id: "actor-1",
      cue_id: null,
      effect_id: null,
      act_id: null,
      tool_call_id: null,
      session_generation: u64("18446744073709551615"),
    };
    const record: TimelineViewRecord = {
      ...RECORD,
      time_range: "viewport",
      scope: "selection",
    };
    const response: TimelineViewResponse = {
      ...RESPONSE,
      binding: {
        ...RESPONSE.binding,
        time_range: "viewport",
        range_start_ns: u64("1"),
        scope: "selection",
        selected_scope: selectedScope,
      },
      rows: RESPONSE.rows.map((row) => ({ ...row, scope: selectedScope })),
    };
    const { container } = renderState({ status: "ready", response }, record);
    const panel = container.querySelector(".timeline-view");
    expect(panel).toHaveAttribute("data-time-binding", "viewport");
    expect(panel).toHaveAttribute("data-scope-binding", "selection");
    const binding = screen.getByLabelText("Timeline binding");
    expect(within(binding).getByText("Viewport")).toBeInTheDocument();
    expect(within(binding).getByText("Selection")).toBeInTheDocument();
    expect(within(binding).getByText("18446744073709551615")).toBeInTheDocument();
  });

  it("keeps custom group and local-error business strings as inert plain text", () => {
    const payload = "<img src=x onerror=globalThis.compromised=true>";
    const customRecord = timelineRecord({
      renderer: "timeline",
      view_schema_version: 1,
      id: "custom_timeline",
      title: "Custom marker timeline",
      time_range: "run",
      scope: "run",
      query: {
        source: {
          source: "instant",
          selector: { selector: "custom", name: "example.marker" },
        },
        filters: [],
        group_by: { dimension: "attribute", key: "label" },
      },
    });
    const customResponse = timelineResponse({
      ...RESPONSE,
      view_id: "custom_timeline",
      rows: [{
        ...RESPONSE.rows[0]!,
        group: {
          dimension: { dimension: "attribute", key: "label" },
          value: { type: "string", value: payload },
        },
        name: "example.marker",
      }],
    }, customRecord);
    const view = renderState({ status: "ready", response: customResponse }, customRecord);
    expect(screen.getByText(payload)).toBeInTheDocument();
    expect(view.container.querySelector("img, script, a")).toBeNull();

    view.rerender(
      <TimelineView
        record={customRecord}
        state={{
          status: "local_error",
          error: { code: "query_failed", message: payload },
        }}
      />,
    );
    expect(view.container.querySelector(".timeline-view")).toHaveAttribute(
      "data-state",
      "local_error",
    );
    expect(screen.getByRole("alert")).toHaveTextContent(payload);
    expect(view.container.querySelector("img, script, a")).toBeNull();
  });

  it("has a static source boundary with no query, Canvas, or sibling renderer coupling", () => {
    const source = readFileSync(resolve(process.cwd(), "src/views/timeline.tsx"), "utf8");
    expect(source).toContain("TimelineViewResponse");
    expect(source).not.toMatch(/MetricViewResponse|TableViewResponse|TimeSeriesViewResponse/);
    expect(source).not.toMatch(/fetch\s*\(|EventSource|XMLHttpRequest|WebSocket/);
    expect(source).not.toMatch(/canvas|getContext|requestAnimationFrame|uPlot/i);
    expect(source).not.toMatch(/dangerouslySetInnerHTML|innerHTML|insertAdjacentHTML/);
  });
});
