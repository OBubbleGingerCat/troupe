import type {
  JsonObject,
  JsonValue,
  U64String,
} from "../protocol/decimal.ts";
import type { ViewCoverage } from "../protocol/view.ts";
import type { TimeSeriesColumnarModel } from "../query/client.ts";


const CANONICAL_INTEGER = /^(?:0|-?[1-9][0-9]*)$/;
const CANONICAL_DECIMAL = /^-?(?:0|[1-9][0-9]*)(?:\.[0-9]*[1-9])?$/;
const SAFE_INTEGER = BigInt(Number.MAX_SAFE_INTEGER);
const MAX_BINARY_DIVISOR_EXPONENT = 1022;

export type TimeSeriesPlotValueReason =
  | "non_binary_exact"
  | "outside_safe_range";

export interface TimeSeriesPlotValue {
  readonly exact_text: string;
  readonly plot_value: number | null;
  readonly reason: TimeSeriesPlotValueReason | null;
}

export interface TimeSeriesPlotPoint extends TimeSeriesPlotValue {
  readonly bucket_start_ns: U64String;
  readonly bucket_end_ns: U64String;
  readonly partial: boolean;
  readonly coverage: ViewCoverage;
}

export interface TimeSeriesPlotSeries {
  readonly group: JsonObject | null;
  readonly label: string;
  readonly points: readonly (TimeSeriesPlotPoint | null)[];
  readonly plot_values: readonly (number | null)[];
}

export interface TimeSeriesTextOnlyValue {
  readonly series_label: string;
  readonly bucket_start_ns: U64String;
  readonly bucket_end_ns: U64String;
  readonly exact_text: string;
  readonly reason: TimeSeriesPlotValueReason;
}

export interface TimeSeriesPlotModel {
  readonly source: TimeSeriesColumnarModel;
  readonly origin_ns: U64String;
  readonly x_values: readonly number[] | null;
  readonly x_end_values: readonly number[] | null;
  readonly series: readonly TimeSeriesPlotSeries[];
  readonly text_only_values: readonly TimeSeriesTextOnlyValue[];
  readonly partial_bucket_count: number;
  readonly timestamp_reason: "outside_safe_range" | null;
  readonly has_plottable_values: boolean;
}

export interface TimeSeriesSelection {
  readonly start_ns: U64String;
  readonly end_ns: U64String;
}

export class TimeSeriesModelError extends Error {
  constructor(message: string) {
    super(message);
    this.name = "TimeSeriesModelError";
  }
}

interface Rational {
  readonly numerator: bigint;
  readonly denominator: bigint;
}

interface ExactNumber extends Rational {
  readonly text: string;
}

function objectValue(value: JsonValue | undefined, label: string): JsonObject {
  if (value === null || typeof value !== "object" || Array.isArray(value)) {
    throw new TimeSeriesModelError(`${label} must be an object`);
  }
  return value as JsonObject;
}

function stringValue(value: JsonValue | undefined, label: string): string {
  if (typeof value !== "string") {
    throw new TimeSeriesModelError(`${label} must be a string`);
  }
  return value;
}

function greatestCommonDivisor(left: bigint, right: bigint): bigint {
  let a = left < 0n ? -left : left;
  let b = right < 0n ? -right : right;
  while (b !== 0n) {
    [a, b] = [b, a % b];
  }
  return a;
}

function canonicalNumber(value: JsonObject, label: string): ExactNumber {
  const kind = stringValue(value.type, `${label}.type`);
  const text = stringValue(value.value, `${label}.value`);
  if (kind === "integer") {
    if (!CANONICAL_INTEGER.test(text)) {
      throw new TimeSeriesModelError(`${label} is not a canonical integer`);
    }
    return { numerator: BigInt(text), denominator: 1n, text };
  }
  if (kind !== "decimal" || !CANONICAL_DECIMAL.test(text) || text === "-0") {
    throw new TimeSeriesModelError(`${label} is not a canonical decimal`);
  }
  const negative = text.startsWith("-");
  const unsigned = negative ? text.slice(1) : text;
  const [whole = "0", fraction = ""] = unsigned.split(".");
  const magnitude = BigInt(`${whole}${fraction}`);
  return {
    numerator: negative ? -magnitude : magnitude,
    denominator: 10n ** BigInt(fraction.length),
    text,
  };
}

function aggregateValue(value: JsonObject): ExactNumber {
  const aggregate = stringValue(value.aggregate, "aggregate.aggregate");
  if (aggregate === "exact") {
    return canonicalNumber(objectValue(value.value, "aggregate.value"), "aggregate.value");
  }
  if (aggregate !== "mean") {
    throw new TimeSeriesModelError("aggregate kind is not exact or mean");
  }
  const numerator = canonicalNumber(
    objectValue(value.numerator, "aggregate.numerator"),
    "aggregate.numerator",
  );
  const countText = stringValue(
    value.contributing_count,
    "aggregate.contributing_count",
  );
  if (!/^[1-9][0-9]*$/.test(countText)) {
    throw new TimeSeriesModelError("mean contributing_count must be positive");
  }
  const count = BigInt(countText);
  return {
    numerator: numerator.numerator,
    denominator: numerator.denominator * count,
    text: `${numerator.text} / ${countText}`,
  };
}

function exactRationalToNumber(value: Rational): {
  readonly value: number | null;
  readonly reason: TimeSeriesPlotValueReason | null;
} {
  if (value.numerator === 0n) {
    return { value: 0, reason: null };
  }
  const divisor = greatestCommonDivisor(value.numerator, value.denominator);
  const numerator = value.numerator / divisor;
  let denominator = value.denominator / divisor;
  let binaryDivisorExponent = 0;
  while (denominator > 1n && denominator % 2n === 0n) {
    denominator /= 2n;
    binaryDivisorExponent += 1;
  }
  if (denominator !== 1n) {
    return { value: null, reason: "non_binary_exact" };
  }
  if (
    numerator < -SAFE_INTEGER
    || numerator > SAFE_INTEGER
    || binaryDivisorExponent > MAX_BINARY_DIVISOR_EXPONENT
  ) {
    return { value: null, reason: "outside_safe_range" };
  }
  const result = Number(numerator) * (2 ** -binaryDivisorExponent);
  if (!Number.isFinite(result) || result === 0) {
    return { value: null, reason: "outside_safe_range" };
  }
  return { value: result, reason: null };
}

function plotValue(value: JsonObject): TimeSeriesPlotValue {
  const exact = aggregateValue(value);
  const converted = exactRationalToNumber(exact);
  return {
    exact_text: exact.text,
    plot_value: converted.value,
    reason: converted.reason,
  };
}

function taggedText(value: JsonObject): string {
  const kind = typeof value.type === "string" ? value.type : "unknown";
  if (kind === "null") {
    return "null";
  }
  const item = value.value;
  return typeof item === "string" || typeof item === "boolean" || typeof item === "number"
    ? String(item)
    : "unknown";
}

export function timeSeriesGroupLabel(group: JsonObject | null): string {
  if (group === null) {
    return "All series";
  }
  const dimension = objectValue(group.dimension, "group.dimension");
  const value = objectValue(group.value, "group.value");
  const name = typeof dimension.dimension === "string"
    ? dimension.dimension
    : "group";
  const key = typeof dimension.key === "string" ? `/${dimension.key}` : "";
  return `${name}${key}: ${taggedText(value)}`;
}

function relativeCoordinate(value: U64String, origin: bigint): number | null {
  const relative = BigInt(value) - origin;
  return relative < 0n || relative > SAFE_INTEGER ? null : Number(relative);
}

export function buildTimeSeriesPlotModel(
  source: TimeSeriesColumnarModel,
): TimeSeriesPlotModel {
  const pointCount = source.bucket_start_ns.length;
  if (
    source.bucket_end_ns.length !== pointCount
    || source.partial.length !== pointCount
    || source.series.some(
      (series) => series.values.length !== pointCount || series.coverage.length !== pointCount,
    )
  ) {
    throw new TimeSeriesModelError("time-series columns are not aligned");
  }

  let previousEnd: bigint | null = null;
  source.bucket_start_ns.forEach((startText, index) => {
    const start = BigInt(startText);
    const end = BigInt(source.bucket_end_ns[index]!);
    if (end <= start || (previousEnd !== null && start !== previousEnd)) {
      throw new TimeSeriesModelError("time-series buckets are not contiguous intervals");
    }
    previousEnd = end;
  });

  const originText = source.bucket_start_ns[0] ?? source.range_start_ns;
  const origin = BigInt(originText);
  const rawX = source.bucket_start_ns.map((value) => relativeCoordinate(value, origin));
  const rawXEnd = source.bucket_end_ns.map((value) => relativeCoordinate(value, origin));
  const timestampsSafe = rawX.every((value) => value !== null)
    && rawXEnd.every((value) => value !== null);
  const xValues = timestampsSafe ? rawX as number[] : null;
  const xEndValues = timestampsSafe ? rawXEnd as number[] : null;

  const textOnlyValues: TimeSeriesTextOnlyValue[] = [];
  const series = source.series.map((column) => {
    const label = timeSeriesGroupLabel(column.group);
    const points = column.values.map((value, index): TimeSeriesPlotPoint | null => {
      if (value === null) {
        return null;
      }
      const converted = plotValue(value);
      const point = {
        ...converted,
        bucket_start_ns: source.bucket_start_ns[index]!,
        bucket_end_ns: source.bucket_end_ns[index]!,
        partial: source.partial[index]!,
        coverage: column.coverage[index]!,
      };
      if (converted.reason !== null) {
        textOnlyValues.push({
          series_label: label,
          bucket_start_ns: point.bucket_start_ns,
          bucket_end_ns: point.bucket_end_ns,
          exact_text: converted.exact_text,
          reason: converted.reason,
        });
      }
      return point;
    });
    return {
      group: column.group,
      label,
      points,
      plot_values: points.map((point) => point?.plot_value ?? null),
    };
  });

  return {
    source,
    origin_ns: originText,
    x_values: xValues,
    x_end_values: xEndValues,
    series,
    text_only_values: textOnlyValues,
    partial_bucket_count: source.partial.filter(Boolean).length,
    timestamp_reason: timestampsSafe ? null : "outside_safe_range",
    has_plottable_values: timestampsSafe
      && series.some((column) => column.plot_values.some((value) => value !== null)),
  };
}

export function selectionFromRelativeRange(
  model: TimeSeriesPlotModel,
  left: number,
  right: number,
): TimeSeriesSelection | null {
  if (
    model.x_values === null
    || model.x_end_values === null
    || !Number.isFinite(left)
    || !Number.isFinite(right)
  ) {
    return null;
  }
  const minimum = Math.min(left, right);
  const maximum = Math.max(left, right);
  const first = model.x_end_values.findIndex((end) => end > minimum);
  if (first < 0) {
    return null;
  }
  let last = first;
  while (last + 1 < model.x_values.length && model.x_values[last + 1]! < maximum) {
    last += 1;
  }
  if (model.x_values[first]! >= maximum) {
    return null;
  }
  return {
    start_ns: model.source.bucket_start_ns[first]!,
    end_ns: model.source.bucket_end_ns[last]!,
  };
}

export function selectionRelativeRange(
  model: TimeSeriesPlotModel,
  selection: TimeSeriesSelection,
): readonly [number, number] | null {
  const firstBucket = model.source.bucket_start_ns[0];
  const lastBucket = model.source.bucket_end_ns[model.source.bucket_end_ns.length - 1];
  if (
    firstBucket === undefined
    || lastBucket === undefined
    || BigInt(selection.start_ns) < BigInt(firstBucket)
    || BigInt(selection.end_ns) > BigInt(lastBucket)
  ) {
    return null;
  }
  const origin = BigInt(model.origin_ns);
  const start = relativeCoordinate(selection.start_ns, origin);
  const end = relativeCoordinate(selection.end_ns, origin);
  return start === null || end === null || start >= end ? null : [start, end];
}
