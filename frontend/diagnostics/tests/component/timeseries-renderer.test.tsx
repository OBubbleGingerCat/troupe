import "@testing-library/jest-dom/vitest";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import {
  type JsonObject,
  decodeU64,
} from "../../src/protocol/decimal.ts";
import type { ViewCoverage } from "../../src/protocol/view.ts";
import type { TimeSeriesColumnarModel } from "../../src/query/client.ts";
import {
  TimeSeriesModelError,
  buildTimeSeriesPlotModel,
  selectionFromRelativeRange,
} from "../../src/timeseries/model.ts";
import { TimeSeriesRenderer } from "../../src/timeseries/renderer.ts";


interface PlotRecord {
  readonly data: readonly unknown[];
  readonly options: Record<string, unknown>;
  readonly sizes: { readonly width: number; readonly height: number }[];
  readonly selections: { readonly left: number; readonly top: number; readonly width: number; readonly height: number }[];
  destroyed: boolean;
  fireSelection(left: number, width: number): void;
}

const plotHarness = vi.hoisted(() => ({ records: [] as PlotRecord[] }));

vi.mock("uplot", () => {
  class MockUPlot {
    readonly bbox = { height: 280 };
    readonly root: HTMLElement;
    readonly record: PlotRecord;
    select = { left: 0, top: 0, width: 0, height: 0 };

    constructor(options: Record<string, unknown>, data: readonly unknown[], mount: HTMLElement) {
      this.root = document.createElement("div");
      this.root.className = "uplot";
      this.root.append(document.createElement("canvas"));
      mount.append(this.root);
      const hooks = options.hooks as { readonly setSelect?: readonly ((plot: MockUPlot) => void)[] } | undefined;
      this.record = {
        data,
        options,
        sizes: [],
        selections: [],
        destroyed: false,
        fireSelection: (left, width) => {
          this.select = { left, top: 0, width, height: 280 };
          hooks?.setSelect?.forEach((hook) => hook(this));
        },
      };
      plotHarness.records.push(this.record);
    }

    setSize(size: { readonly width: number; readonly height: number }): void {
      this.record.sizes.push(size);
    }

    setSelect(
      selection: { readonly left: number; readonly top: number; readonly width: number; readonly height: number },
    ): void {
      this.select = { ...selection };
      this.record.selections.push(selection);
    }

    valToPos(value: number): number {
      return value * 10;
    }

    posToVal(value: number): number {
      return value / 10;
    }

    destroy(): void {
      this.record.destroyed = true;
      this.root.remove();
    }
  }

  return { default: MockUPlot };
});

class TestResizeObserver implements ResizeObserver {
  static readonly instances: TestResizeObserver[] = [];
  readonly #callback: ResizeObserverCallback;
  readonly observed = new Set<Element>();
  disconnected = false;

  constructor(callback: ResizeObserverCallback) {
    this.#callback = callback;
    TestResizeObserver.instances.push(this);
  }

  observe(target: Element): void {
    this.observed.add(target);
  }

  unobserve(target: Element): void {
    this.observed.delete(target);
  }

  disconnect(): void {
    this.disconnected = true;
    this.observed.clear();
  }

  takeRecords(): ResizeObserverEntry[] {
    return [];
  }

  emit(width: number): void {
    const target = [...this.observed][0];
    if (target === undefined) {
      return;
    }
    this.#callback([{
      target,
      contentRect: { width } as DOMRectReadOnly,
    } as ResizeObserverEntry], this);
  }
}

let animationFrames = new Map<number, FrameRequestCallback>();
let nextAnimationFrame = 1;

function flushAnimationFrames(): void {
  const callbacks = [...animationFrames.values()];
  animationFrames = new Map();
  callbacks.forEach((callback) => callback(0));
}

function u64(value: string) {
  return decodeU64(value);
}

function coverage({
  status = "complete",
  contributing = "1",
  gaps = "0",
}: {
  readonly status?: ViewCoverage["status"];
  readonly contributing?: string;
  readonly gaps?: string;
} = {}): ViewCoverage {
  return {
    status,
    matched_count: u64(contributing),
    contributing_count: u64(contributing),
    excluded_count: u64("0"),
    excluded: {
      open_spans: u64("0"),
      missing_values: u64("0"),
      non_numeric_values: u64("0"),
      unavailable_values: u64("0"),
      resource_truncated: u64("0"),
    },
    gap_count: u64(gaps),
  };
}

function exactInteger(value: string): JsonObject {
  return { aggregate: "exact", value: { type: "integer", value } };
}

function exactDecimal(value: string): JsonObject {
  return { aggregate: "exact", value: { type: "decimal", value } };
}

function exactMean(numerator: string, contributingCount: string): JsonObject {
  return {
    aggregate: "mean",
    numerator: { type: "integer", value: numerator },
    contributing_count: contributingCount,
  };
}

function columnar(
  values: readonly (JsonObject | null)[],
  starts = values.map((_, index) => String(index)),
  ends = values.map((_, index) => String(index + 1)),
): TimeSeriesColumnarModel {
  const lastEnd = ends[ends.length - 1];
  return {
    range_start_ns: u64(starts[0] ?? "0"),
    range_end_ns: u64(lastEnd ?? starts[0] ?? "0"),
    captured_watermark: u64("7"),
    captured_elapsed_end_ns: u64(lastEnd ?? "0"),
    bucket_width_ns: u64("1"),
    bucket_start_ns: starts.map(u64),
    bucket_end_ns: ends.map(u64),
    partial: values.map((_, index) => index === 0),
    series: [{
      group: null,
      values,
      coverage: values.map((value) => coverage({ contributing: value === null ? "0" : "1" })),
    }],
    coverage: coverage({ contributing: String(values.filter((value) => value !== null).length) }),
    truncated: false,
  };
}

beforeEach(() => {
  plotHarness.records.length = 0;
  TestResizeObserver.instances.length = 0;
  animationFrames = new Map();
  nextAnimationFrame = 1;
  vi.stubGlobal("ResizeObserver", TestResizeObserver);
  vi.stubGlobal("requestAnimationFrame", (callback: FrameRequestCallback) => {
    const id = nextAnimationFrame;
    nextAnimationFrame += 1;
    animationFrames.set(id, callback);
    return id;
  });
  vi.stubGlobal("cancelAnimationFrame", (id: number) => {
    animationFrames.delete(id);
  });
});

afterEach(() => {
  vi.unstubAllGlobals();
  document.body.replaceChildren();
});

describe("W13 time-series renderer", () => {
  it("plots only values that satisfy the explicit exact numeric policy", () => {
    const model = buildTimeSeriesPlotModel(columnar([
      exactInteger("1"),
      exactDecimal("0.5"),
      exactInteger("9007199254740993"),
      exactDecimal("0.1"),
      exactMean("3", "2"),
    ]));

    expect(model.x_values).toEqual([0, 1, 2, 3, 4]);
    expect(model.series[0]?.plot_values).toEqual([1, 0.5, null, null, 1.5]);
    expect(model.text_only_values).toEqual([
      expect.objectContaining({ exact_text: "9007199254740993", reason: "outside_safe_range" }),
      expect.objectContaining({ exact_text: "0.1", reason: "non_binary_exact" }),
    ]);
    expect(model.series[0]?.plot_values).not.toContain(0);
  });

  it("uses exact relative bigint coordinates and rejects an unsafe duration", () => {
    const nearLimit = buildTimeSeriesPlotModel(columnar(
      [exactInteger("1"), exactInteger("2")],
      ["18446744073709550000", "18446744073709550001"],
      ["18446744073709550001", "18446744073709550002"],
    ));
    expect(nearLimit.origin_ns).toBe("18446744073709550000");
    expect(nearLimit.x_values).toEqual([0, 1]);

    const unsafe = buildTimeSeriesPlotModel(columnar(
      [exactInteger("1")],
      ["0"],
      ["9007199254740992"],
    ));
    expect(unsafe.timestamp_reason).toBe("outside_safe_range");
    expect(unsafe.x_values).toBeNull();
    expect(unsafe.has_plottable_values).toBe(false);
  });

  it("rejects misaligned columns instead of rebucketing them", () => {
    const source = columnar([exactInteger("1"), exactInteger("2")]);
    expect(() => buildTimeSeriesPlotModel({
      ...source,
      bucket_end_ns: [u64("1")],
    })).toThrow(TimeSeriesModelError);
  });

  it("snaps plot selection to exact server bucket boundaries", () => {
    const model = buildTimeSeriesPlotModel(columnar([
      exactInteger("1"),
      exactInteger("2"),
      exactInteger("3"),
    ]));
    expect(selectionFromRelativeRange(model, 0.2, 1.8)).toEqual({
      start_ns: "0",
      end_ns: "2",
    });
    expect(selectionFromRelativeRange(model, 4, 5)).toBeNull();
  });

  it("renders exact fallback text and cleans resize, selection, theme, and plot state", () => {
    const host = document.createElement("div");
    document.body.append(host);
    const selected: unknown[] = [];
    const source = columnar([
      exactInteger("1"),
      exactInteger("9007199254740993"),
      exactInteger("2"),
    ]);
    const renderer = new TimeSeriesRenderer(host, {
      model: source,
      title: "Queue depth",
      theme: "light",
      onSelectionChange: (selection) => selected.push(selection),
    });

    flushAnimationFrames();
    expect(plotHarness.records).toHaveLength(1);
    expect(plotHarness.records[0]?.data).toEqual([[0, 1, 2], [1, null, 2]]);
    expect(host.querySelector("canvas")).not.toBeNull();
    expect(host.textContent).toContain("9007199254740993");
    expect(host.textContent).toContain("Text-only values1");
    expect(TestResizeObserver.instances[0]?.disconnected).toBe(false);

    renderer.setSelection({ start_ns: u64("0"), end_ns: u64("2") });
    expect(host.querySelector(".timeseries-renderer")?.getAttribute("data-selection-start-ns")).toBe("0");
    plotHarness.records[0]?.fireSelection(2, 16);
    expect(selected).toEqual([{ start_ns: "0", end_ns: "2" }]);

    renderer.setSelection({ start_ns: u64("0"), end_ns: u64("7") });
    expect(host.querySelector(".timeseries-renderer")).not.toHaveAttribute("data-selection-start-ns");
    expect(() => renderer.update({
      model: { ...source, bucket_end_ns: [u64("1")] },
      title: "Invalid replacement",
    })).toThrow(TimeSeriesModelError);
    expect(plotHarness.records[0]?.destroyed).toBe(false);

    renderer.update({ model: source, title: "Queue depth", theme: "dark" });
    expect(plotHarness.records[0]?.destroyed).toBe(true);
    expect(TestResizeObserver.instances[0]?.disconnected).toBe(true);
    expect(host.querySelector(".timeseries-renderer")).toHaveAttribute("data-theme", "dark");

    renderer.destroy();
    expect(plotHarness.records[1]?.destroyed).toBe(true);
    expect(TestResizeObserver.instances[1]?.disconnected).toBe(true);
    expect(host).toBeEmptyDOMElement();
    renderer.destroy();
  });

  it("keeps empty, partial, and gap coverage visible without creating a plot", () => {
    const host = document.createElement("div");
    document.body.append(host);
    const empty = columnar([null]);
    const renderer = new TimeSeriesRenderer(host, {
      model: {
        ...empty,
        coverage: coverage({ status: "partial", contributing: "0", gaps: "2" }),
      },
      title: "Empty series",
    });

    const root = host.querySelector(".timeseries-renderer");
    expect(root).toHaveAttribute("data-plot-state", "empty");
    expect(root).toHaveAttribute("data-coverage", "gap");
    expect(root).toHaveTextContent("No time-series values in this range.");
    expect(root).toHaveTextContent("Observation gaps2");
    expect(root).toHaveTextContent("Partial buckets1");
    expect(host.querySelector("canvas")).toBeNull();
    expect(plotHarness.records).toHaveLength(0);

    renderer.destroy();
    expect(host).toBeEmptyDOMElement();
  });
});
