import uPlot from "uplot";
import "uplot/dist/uPlot.min.css";

import type { U64String } from "../protocol/decimal.ts";
import type { TimeSeriesColumnarModel } from "../query/client.ts";
import {
  type TimeSeriesPlotModel,
  type TimeSeriesSelection,
  buildTimeSeriesPlotModel,
  selectionFromRelativeRange,
  selectionRelativeRange,
} from "./model.ts";
import {
  TimeSeriesResizeController,
  normalizeTimeSeriesPlotSize,
} from "./resize.ts";


const TEXT_ONLY_DETAIL_LIMIT = 256;
const LIGHT_COLORS = [
  "#166534",
  "#b45309",
  "#1d4ed8",
  "#be123c",
  "#0f766e",
  "#7e22ce",
  "#4d7c0f",
  "#c2410c",
] as const;
const DARK_COLORS = [
  "#4ade80",
  "#fbbf24",
  "#60a5fa",
  "#fb7185",
  "#2dd4bf",
  "#c084fc",
  "#a3e635",
  "#fb923c",
] as const;

export type TimeSeriesTheme = "light" | "dark";

export interface TimeSeriesRendererOptions {
  readonly model: TimeSeriesColumnarModel;
  readonly title: string;
  readonly theme?: TimeSeriesTheme;
  readonly selection?: TimeSeriesSelection | null;
  readonly onSelectionChange?: (selection: TimeSeriesSelection | null) => void;
}

interface ActivePlot {
  readonly plot: uPlot;
  readonly resize: TimeSeriesResizeController;
  readonly model: TimeSeriesPlotModel;
}

function node<K extends keyof HTMLElementTagNameMap>(
  tag: K,
  className: string,
  text?: string,
): HTMLElementTagNameMap[K] {
  const element = document.createElement(tag);
  element.className = className;
  if (text !== undefined) {
    element.textContent = text;
  }
  return element;
}

function exactCode(value: U64String | string): HTMLElement {
  return node("code", "timeseries-renderer__exact", value);
}

function coverageState(model: TimeSeriesPlotModel): string {
  if (model.source.truncated) {
    return "truncated";
  }
  if (model.source.coverage.gap_count !== "0") {
    return "gap";
  }
  return model.source.coverage.status;
}

function plotState(model: TimeSeriesPlotModel): string {
  const exactValueCount = model.series.reduce(
    (count, series) => count + series.points.filter((point) => point !== null).length,
    0,
  );
  if (exactValueCount === 0) {
    return "empty";
  }
  if (model.timestamp_reason !== null) {
    return "coordinate_unavailable";
  }
  return model.has_plottable_values ? "ready" : "text_only";
}

function addDefinition(list: HTMLDListElement, term: string, value: string): void {
  const item = node("div", "timeseries-renderer__coverage-item");
  item.append(node("dt", "timeseries-renderer__coverage-term", term));
  const description = node("dd", "timeseries-renderer__coverage-value");
  description.append(exactCode(value));
  item.append(description);
  list.append(item);
}

function coverageElement(model: TimeSeriesPlotModel): HTMLElement {
  const section = node("section", "timeseries-renderer__coverage");
  section.setAttribute("aria-label", "Time series coverage");
  section.dataset.status = coverageState(model);
  const heading = node("h5", "timeseries-renderer__section-title", "Coverage");
  const status = node(
    "p",
    "timeseries-renderer__coverage-status",
    coverageState(model),
  );
  status.setAttribute("role", "status");
  const list = node("dl", "timeseries-renderer__coverage-list");
  addDefinition(list, "Matched", model.source.coverage.matched_count);
  addDefinition(list, "Contributing", model.source.coverage.contributing_count);
  addDefinition(list, "Excluded", model.source.coverage.excluded_count);
  addDefinition(list, "Observation gaps", model.source.coverage.gap_count);
  addDefinition(list, "Partial buckets", String(model.partial_bucket_count));
  addDefinition(list, "Text-only values", String(model.text_only_values.length));
  section.append(heading, status, list);
  return section;
}

function legendElement(model: TimeSeriesPlotModel, theme: TimeSeriesTheme): HTMLElement {
  const section = node("section", "timeseries-renderer__legend");
  section.setAttribute("aria-label", "Time series legend");
  section.append(node("h5", "timeseries-renderer__section-title", "Series"));
  const list = node("ul", "timeseries-renderer__legend-list");
  const colors = theme === "dark" ? DARK_COLORS : LIGHT_COLORS;
  model.series.forEach((series, index) => {
    const item = node("li", "timeseries-renderer__legend-item");
    const swatch = node("span", "timeseries-renderer__swatch");
    swatch.setAttribute("aria-hidden", "true");
    swatch.style.backgroundColor = colors[index % colors.length]!;
    item.append(swatch, document.createTextNode(series.label));
    list.append(item);
  });
  section.append(list);
  return section;
}

function textOnlyElement(model: TimeSeriesPlotModel): HTMLElement | null {
  const total = model.text_only_values.length;
  if (total === 0) {
    return null;
  }
  const details = node("details", "timeseries-renderer__text-only");
  details.open = true;
  details.dataset.total = String(total);
  details.append(node(
    "summary",
    "timeseries-renderer__text-only-summary",
    `${total} exact ${total === 1 ? "value is" : "values are"} text-only`,
  ));
  const list = node("ol", "timeseries-renderer__text-only-list");
  model.text_only_values.slice(0, TEXT_ONLY_DETAIL_LIMIT).forEach((value) => {
    const item = node("li", "timeseries-renderer__text-only-item");
    const identity = node(
      "span",
      "timeseries-renderer__text-only-identity",
      `${value.series_label}, ${value.bucket_start_ns}-${value.bucket_end_ns} ns: `,
    );
    item.append(identity, exactCode(value.exact_text));
    item.append(document.createTextNode(
      value.reason === "non_binary_exact"
        ? " (not exactly representable as a binary plot value)"
        : " (outside the exact plotting range)",
    ));
    list.append(item);
  });
  details.append(list);
  if (total > TEXT_ONLY_DETAIL_LIMIT) {
    details.append(node(
      "p",
      "timeseries-renderer__text-only-remainder",
      `${total - TEXT_ONLY_DETAIL_LIMIT} additional text-only values are included in coverage.`,
    ));
  }
  return details;
}

function plotData(model: TimeSeriesPlotModel): uPlot.AlignedData {
  if (model.x_values === null) {
    return [[]];
  }
  return [
    [...model.x_values],
    ...model.series.map((series) => [...series.plot_values]),
  ] as uPlot.AlignedData;
}

function plotOptions(
  model: TimeSeriesPlotModel,
  theme: TimeSeriesTheme,
  onSelect: (plot: uPlot) => void,
  width: number,
  height: number,
): uPlot.Options {
  const colors = theme === "dark" ? DARK_COLORS : LIGHT_COLORS;
  return {
    width,
    height,
    padding: [12, 12, 0, 0],
    legend: { show: false },
    scales: { x: { time: false } },
    cursor: {
      drag: { x: true, y: false, setScale: false },
    },
    axes: [
      {
        label: `Elapsed from ${model.origin_ns} ns`,
        values: (_plot, values) => values.map((value) => `+${String(value)} ns`),
        stroke: theme === "dark" ? "#d1d5db" : "#374151",
        grid: { stroke: theme === "dark" ? "#374151" : "#e5e7eb", width: 1 },
      },
      {
        stroke: theme === "dark" ? "#d1d5db" : "#374151",
        grid: { stroke: theme === "dark" ? "#374151" : "#e5e7eb", width: 1 },
      },
    ],
    series: [
      {},
      ...model.series.map((series, index) => ({
        label: series.label,
        stroke: colors[index % colors.length]!,
        width: 2,
        spanGaps: false,
        points: { show: true, size: 5 },
      })),
    ],
    hooks: { setSelect: [onSelect] },
  };
}

function applyRootStyles(root: HTMLElement, theme: TimeSeriesTheme): void {
  Object.assign(root.style, {
    color: theme === "dark" ? "#f3f4f6" : "#111827",
    background: theme === "dark" ? "#171717" : "#ffffff",
    display: "grid",
    gap: "12px",
    letterSpacing: "0",
    minWidth: "0",
    overflowWrap: "anywhere",
    width: "100%",
  });
}

export class TimeSeriesRenderer {
  readonly #host: HTMLElement;
  #root: HTMLElement | null = null;
  #active: ActivePlot | null = null;
  #selectionCallback: ((selection: TimeSeriesSelection | null) => void) | null = null;
  #suppressSelection = false;
  #destroyed = false;

  constructor(host: HTMLElement, options: TimeSeriesRendererOptions) {
    this.#host = host;
    this.update(options);
  }

  update(options: TimeSeriesRendererOptions): void {
    if (this.#destroyed) {
      throw new Error("time-series renderer is destroyed");
    }
    const model = buildTimeSeriesPlotModel(options.model);
    const theme = options.theme ?? "light";
    this.#disposePlot();
    this.#selectionCallback = options.onSelectionChange ?? null;

    const root = node("section", "timeseries-renderer");
    root.setAttribute("aria-label", `${options.title} time series`);
    root.dataset.theme = theme;
    root.dataset.coverage = coverageState(model);
    root.dataset.plotState = plotState(model);
    root.dataset.textOnlyValues = String(model.text_only_values.length);
    applyRootStyles(root, theme);

    const header = node("header", "timeseries-renderer__header");
    const title = node("h4", "timeseries-renderer__title", options.title);
    Object.assign(title.style, { fontSize: "14px", lineHeight: "20px", margin: "0" });
    const range = node("p", "timeseries-renderer__range");
    range.append(
      document.createTextNode("Range "),
      exactCode(options.model.range_start_ns),
      document.createTextNode(" to "),
      exactCode(options.model.range_end_ns),
      document.createTextNode(" ns; origin "),
      exactCode(model.origin_ns),
      document.createTextNode(" ns"),
    );
    header.append(title, range);
    root.append(header, coverageElement(model));

    const plotMount = node("div", "timeseries-renderer__plot");
    plotMount.setAttribute("role", "img");
    plotMount.setAttribute("aria-label", `${options.title} plot`);
    Object.assign(plotMount.style, {
      height: "280px",
      minHeight: "220px",
      minWidth: "0",
      overflow: "hidden",
      position: "relative",
      width: "100%",
    });
    if (model.has_plottable_values) {
      root.append(plotMount);
    } else {
      const unavailable = node(
        "p",
        "timeseries-renderer__plot-unavailable",
        plotState(model) === "empty"
          ? "No time-series values in this range."
          : plotState(model) === "text_only"
            ? "Exact values are available below but are not plotted."
            : "The exact time range is outside the plotting coordinate policy.",
      );
      unavailable.setAttribute("role", "status");
      root.append(unavailable);
    }
    if (model.series.length > 0) {
      root.append(legendElement(model, theme));
    }
    const textOnly = textOnlyElement(model);
    if (textOnly !== null) {
      root.append(textOnly);
    }

    this.#host.replaceChildren(root);
    this.#root = root;
    if (model.has_plottable_values) {
      const initial = normalizeTimeSeriesPlotSize(plotMount.getBoundingClientRect().width);
      const plot = new uPlot(
        plotOptions(
          model,
          theme,
          (instance) => this.#selectionChanged(instance),
          initial.width,
          initial.height,
        ),
        plotData(model),
        plotMount,
      );
      let resize: TimeSeriesResizeController;
      try {
        resize = new TimeSeriesResizeController(
          plotMount,
          (size) => plot.setSize(size),
        );
      } catch (error) {
        plot.destroy();
        throw error;
      }
      this.#active = { plot, resize, model };
      this.setSelection(options.selection ?? null);
    } else {
      this.#setSelectionData(options.selection ?? null);
    }
  }

  setSelection(selection: TimeSeriesSelection | null): void {
    if (this.#destroyed) {
      return;
    }
    const active = this.#active;
    if (active === null) {
      this.#setSelectionData(null);
      return;
    }
    const range = selection === null
      ? null
      : selectionRelativeRange(active.model, selection);
    this.#setSelectionData(range === null ? null : selection);
    this.#suppressSelection = true;
    try {
      if (range === null) {
        active.plot.setSelect({ left: 0, top: 0, width: 0, height: 0 }, false);
      } else {
        const left = active.plot.valToPos(range[0], "x");
        const right = active.plot.valToPos(range[1], "x");
        active.plot.setSelect({
          left: Math.min(left, right),
          top: 0,
          width: Math.abs(right - left),
          height: active.plot.bbox.height,
        }, false);
      }
    } finally {
      this.#suppressSelection = false;
    }
  }

  #selectionChanged(plot: uPlot): void {
    if (this.#suppressSelection) {
      return;
    }
    const model = this.#active?.model;
    if (model === undefined) {
      return;
    }
    const selection = plot.select.width <= 1
      ? null
      : selectionFromRelativeRange(
        model,
        plot.posToVal(plot.select.left, "x"),
        plot.posToVal(plot.select.left + plot.select.width, "x"),
      );
    this.#setSelectionData(selection);
    this.#selectionCallback?.(selection);
  }

  #setSelectionData(selection: TimeSeriesSelection | null): void {
    if (this.#root === null) {
      return;
    }
    if (selection === null) {
      delete this.#root.dataset.selectionStartNs;
      delete this.#root.dataset.selectionEndNs;
    } else {
      this.#root.dataset.selectionStartNs = selection.start_ns;
      this.#root.dataset.selectionEndNs = selection.end_ns;
    }
  }

  #disposePlot(): void {
    this.#active?.resize.disconnect();
    this.#active?.plot.destroy();
    this.#active = null;
  }

  destroy(): void {
    if (this.#destroyed) {
      return;
    }
    this.#destroyed = true;
    this.#disposePlot();
    this.#selectionCallback = null;
    this.#root = null;
    this.#host.replaceChildren();
  }
}
