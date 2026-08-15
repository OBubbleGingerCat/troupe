import {
  type CanonicalUuid,
  type JsonObject,
  type U64String,
  decodeCanonicalInteger,
  decodeCanonicalUuid,
  decodeDecimal,
  decodeTokenInteger,
  decodeU64,
  expectArray,
  expectBoolean,
  expectEnum,
  expectExactFields,
  expectInteger,
  expectObject,
  expectString,
  failProtocol,
} from "./decimal.ts";
import {
  COUNTER_KINDS,
  DIAGNOSTIC_EVENT_KINDS,
  INSTANT_KINDS,
  SPAN_KINDS,
  type DiagnosticScope,
  type TaggedScalar,
  decodeDiagnosticScope,
} from "./event.ts";


export const VIEW_RENDERERS = ["timeline", "metric", "table", "time_series"] as const;
export type ViewRenderer = typeof VIEW_RENDERERS[number];

const VIEW_TIME_RANGES = ["viewport", "run"] as const;
const VIEW_SCOPES = ["selection", "run"] as const;
const VIEW_REDUCERS = ["count", "sum", "min", "max", "mean", "latest"] as const;
const TOKEN_METRICS = [
  "provider_total_tokens",
  "input_tokens",
  "output_tokens",
  "thought_tokens",
  "cached_read_tokens",
  "cached_write_tokens",
] as const;
const SPAN_OUTCOMES = ["completed", "cancelled", "failed"] as const;
const SEVERITIES = ["debug", "info", "warning", "error"] as const;
const GROUP_DIMENSIONS = [
  "scene",
  "actor",
  "cue",
  "act",
  "event_name",
  "custom_name",
  "attribute",
  "custom_dimension",
] as const;
const TABLE_COLUMNS = [
  "sequence",
  "elapsed_ns",
  "event_kind",
  "span_kind",
  "instant_kind",
  "counter_kind",
  "scene_id",
  "actor_id",
  "cue_id",
  "act_id",
  "custom_name",
  "outcome",
  "severity",
  "attribute",
  "token",
  "value",
] as const;

export interface ViewRecordBase<R extends ViewRenderer> {
  readonly renderer: R;
  readonly view_schema_version: 1;
  readonly id: string;
  readonly title: string;
  readonly time_range: typeof VIEW_TIME_RANGES[number];
  readonly scope: typeof VIEW_SCOPES[number];
  readonly query: JsonObject;
}

export type TimelineViewRecord = ViewRecordBase<"timeline">;
export type MetricViewRecord = ViewRecordBase<"metric">;
export type TableViewRecord = ViewRecordBase<"table">;
export type TimeSeriesViewRecord = ViewRecordBase<"time_series">;
export type ViewRecord =
  | TimelineViewRecord
  | MetricViewRecord
  | TableViewRecord
  | TimeSeriesViewRecord;

export interface ViewCapabilities {
  readonly event_schema_version: 1;
  readonly view_schema_version: 1;
  readonly api_schema_version: 1;
  readonly max_page_rows: 500;
  readonly max_metric_series: 64;
  readonly max_time_series_points: 1024;
  readonly max_time_series_series: 64;
  readonly bucket_origin: "run";
  readonly interval_semantics: "left_closed_right_open";
  readonly counter_selection: "latest_before_reduce";
  readonly exact_mean_components: true;
}

export interface ViewBinding {
  readonly captured_watermark: U64String;
  readonly captured_elapsed_end_ns: U64String;
  readonly time_range: typeof VIEW_TIME_RANGES[number];
  readonly range_start_ns: U64String;
  readonly range_end_ns: U64String;
  readonly scope: typeof VIEW_SCOPES[number];
  readonly selected_scope: DiagnosticScope | null;
}

export interface ViewCoverage {
  readonly status: "complete" | "partial" | "unavailable";
  readonly matched_count: U64String;
  readonly contributing_count: U64String;
  readonly excluded_count: U64String;
  readonly excluded: {
    readonly open_spans: U64String;
    readonly missing_values: U64String;
    readonly non_numeric_values: U64String;
    readonly unavailable_values: U64String;
    readonly resource_truncated: U64String;
  };
  readonly gap_count: U64String;
}

export interface ViewResponseBase<R extends ViewRenderer> {
  readonly renderer: R;
  readonly api_schema_version: 1;
  readonly view_schema_version: 1;
  readonly run_id: CanonicalUuid;
  readonly view_id: string;
  readonly binding: ViewBinding;
  readonly coverage: ViewCoverage;
  readonly pagination: { readonly page_size: number; readonly next_cursor: string | null } | null;
  readonly truncated: boolean;
  readonly incompatible: {
    readonly reason: "newer_view_schema" | "corrupt_record";
    readonly supported_view_schema_version: 1;
    readonly record_view_schema_version: number | null;
  } | null;
  readonly capabilities: ViewCapabilities;
}

export interface TimelineViewResponse extends ViewResponseBase<"timeline"> {
  readonly rows: readonly {
    readonly sequence: U64String;
    readonly group: JsonObject | null;
    readonly item_type: "span" | "instant";
    readonly name: string;
    readonly start_ns: U64String;
    readonly end_ns: U64String | null;
    readonly scope: DiagnosticScope;
    readonly outcome: typeof SPAN_OUTCOMES[number] | null;
  }[];
}

export interface MetricViewResponse extends ViewResponseBase<"metric"> {
  readonly series: readonly {
    readonly group: JsonObject | null;
    readonly value: JsonObject | null;
    readonly coverage: ViewCoverage;
  }[];
}

export interface TableViewResponse extends ViewResponseBase<"table"> {
  readonly columns: readonly JsonObject[];
  readonly rows: readonly {
    readonly sequence: U64String;
    readonly cells: readonly (TaggedScalar | null)[];
  }[];
}

export interface TimeSeriesViewResponse extends ViewResponseBase<"time_series"> {
  readonly bucket_width_ns: U64String;
  readonly series: readonly {
    readonly group: JsonObject | null;
    readonly points: readonly {
      readonly bucket_start_ns: U64String;
      readonly bucket_end_ns: U64String;
      readonly partial: boolean;
      readonly value: JsonObject | null;
      readonly coverage: ViewCoverage;
    }[];
  }[];
}

export type ViewResponse =
  | TimelineViewResponse
  | MetricViewResponse
  | TableViewResponse
  | TimeSeriesViewResponse;

export type ArchivedViewRecordResult =
  | { readonly status: "compatible"; readonly record: ViewRecord }
  | {
    readonly status: "incompatible";
    readonly reason: "newer_view_schema" | "corrupt_record";
    readonly supported_view_schema_version: 1;
    readonly record_view_schema_version: number | null;
    readonly raw: unknown;
  };

function optional<T>(value: unknown, decoder: (item: unknown, path: string) => T, path: string): T | null {
  return value === null ? null : decoder(value, path);
}

function utf8ByteLength(value: string): number {
  let bytes = 0;
  for (const character of value) {
    const point = character.codePointAt(0)!;
    bytes += point <= 0x7f ? 1 : point <= 0x7ff ? 2 : point <= 0xffff ? 3 : 4;
  }
  return bytes;
}

function decodeViewId(value: unknown, path: string): string {
  const text = expectString(value, path);
  if (!/^[a-z][a-z0-9_]*$/.test(text) || utf8ByteLength(text) > 64) {
    failProtocol("view_id", path, "expected canonical view identifier with at most 64 bytes");
  }
  return text;
}

function hasUnsafeTextMarker(text: string): boolean {
  const lower = text.toLowerCase();
  return ["<", ">", "`"].some((marker) => text.includes(marker))
    || ["javascript:", "data:text/html", "http://", "https://", "url(", "@import"]
      .some((marker) => lower.includes(marker));
}

function decodePlainTitle(value: unknown, path: string): string {
  const text = expectString(value, path);
  if (
    text.length === 0
    || utf8ByteLength(text) > 128
    || /\p{Cc}/u.test(text)
    || hasUnsafeTextMarker(text)
  ) {
    failProtocol("title", path, "plain-text title is unsafe or out of bounds");
  }
  return text;
}

function validateCustomKey(value: unknown, path: string): string {
  const key = expectString(value, path);
  if (
    key.length === 0
    || utf8ByteLength(key) > 64
    || /\p{Cc}/u.test(key)
    || hasUnsafeTextMarker(key)
  ) {
    failProtocol("custom_key", path, "key is unsafe or out of bounds");
  }
  return key;
}

function validateCustomName(value: unknown, path: string): string {
  const name = expectString(value, path);
  const segments = name.split(".");
  if (
    !/^[\x00-\x7f]+$/.test(name)
    || name.length === 0
    || utf8ByteLength(name) > 128
    || segments.length < 2
    || segments[0] === "troupe"
    || segments.some((segment) => !/^[a-z][a-z0-9_]*$/.test(segment))
  ) {
    failProtocol("custom_name", path, "custom name is invalid or reserved");
  }
  return name;
}

function validateTaggedScalar(value: unknown, path: string, allowNull = true): void {
  const tagged = expectObject(value, path);
  const types = allowNull
    ? ["null", "boolean", "integer", "decimal", "string"] as const
    : ["boolean", "integer", "decimal", "string"] as const;
  const kind = expectEnum(tagged.type, types, `${path}.type`);
  expectExactFields(tagged, kind === "null" ? ["type"] : ["type", "value"], path);
  if (kind === "boolean") {
    expectBoolean(tagged.value, `${path}.value`);
  } else if (kind === "integer") {
    decodeCanonicalInteger(tagged.value, `${path}.value`);
  } else if (kind === "decimal") {
    decodeDecimal(tagged.value, `${path}.value`);
  } else if (kind === "string") {
    expectString(tagged.value, `${path}.value`);
  }
}

function validateSelector(value: unknown, path: string, builtIns: readonly string[]): "built_in" | "custom" {
  const selector = expectObject(value, path);
  const kind = expectEnum(selector.selector, ["built_in", "custom"], `${path}.selector`);
  if (kind === "built_in") {
    expectExactFields(selector, ["selector", "kind"], path);
    expectEnum(selector.kind, builtIns, `${path}.kind`);
  } else {
    expectExactFields(selector, ["selector", "name"], path);
    validateCustomName(selector.name, `${path}.name`);
  }
  return kind;
}

function validateGroupDimension(value: unknown, path: string): void {
  const group = expectObject(value, path);
  const dimension = expectEnum(group.dimension, GROUP_DIMENSIONS, `${path}.dimension`);
  if (dimension === "attribute" || dimension === "custom_dimension") {
    expectExactFields(group, ["dimension", "key"], path);
    validateCustomKey(group.key, `${path}.key`);
  } else {
    expectExactFields(group, ["dimension"], path);
  }
}

function validateFilter(value: unknown, path: string): void {
  const filter = expectObject(value, path);
  const kind = expectEnum(
    filter.filter,
    ["severity", "outcome", "attribute_equals", "attribute_exists"],
    `${path}.filter`,
  );
  if (kind === "severity") {
    expectExactFields(filter, ["filter", "value"], path);
    expectEnum(filter.value, SEVERITIES, `${path}.value`);
  } else if (kind === "outcome") {
    expectExactFields(filter, ["filter", "value"], path);
    expectEnum(filter.value, SPAN_OUTCOMES, `${path}.value`);
  } else if (kind === "attribute_equals") {
    expectExactFields(filter, ["filter", "key", "value"], path);
    validateCustomKey(filter.key, `${path}.key`);
    validateTaggedScalar(filter.value, `${path}.value`);
  } else {
    expectExactFields(filter, ["filter", "key"], path);
    validateCustomKey(filter.key, `${path}.key`);
  }
}

function validateFilters(value: unknown, path: string): void {
  const filters = expectArray(value, path);
  if (filters.length > 32) {
    failProtocol("filters", path, "more than 32 filters");
  }
  filters.forEach((filter, index) => validateFilter(filter, `${path}[${index}]`));
}

function validateTimelineSource(value: unknown, path: string): void {
  const source = expectObject(value, path);
  const kind = expectEnum(source.source, ["span", "instant"], `${path}.source`);
  expectExactFields(source, ["source", "selector"], path);
  validateSelector(source.selector, `${path}.selector`, kind === "span" ? SPAN_KINDS : INSTANT_KINDS);
}

function validateMetricSource(value: unknown, path: string): string {
  const source = expectObject(value, path);
  const kind = expectEnum(
    source.source,
    ["counter_value", "instant_count", "completed_span_duration", "act_token"],
    `${path}.source`,
  );
  if (kind === "counter_value") {
    expectExactFields(source, ["source", "selector", "selection"], path);
    validateSelector(source.selector, `${path}.selector`, COUNTER_KINDS);
    if (source.selection !== "latest_before_reduce") {
      failProtocol("counter_selection", `${path}.selection`, "expected latest_before_reduce");
    }
  } else if (kind === "instant_count") {
    expectExactFields(source, ["source", "selector"], path);
    validateSelector(source.selector, `${path}.selector`, INSTANT_KINDS);
  } else if (kind === "completed_span_duration") {
    expectExactFields(source, ["source", "selector"], path);
    validateSelector(source.selector, `${path}.selector`, SPAN_KINDS);
  } else {
    expectExactFields(source, ["source", "metric"], path);
    expectEnum(source.metric, TOKEN_METRICS, `${path}.metric`);
  }
  return kind;
}

function validateTableSource(value: unknown, path: string): void {
  const source = expectObject(value, path);
  const kind = expectEnum(
    source.source,
    ["event", "span", "instant", "counter", "act_token_usage"],
    `${path}.source`,
  );
  if (kind === "event") {
    expectExactFields(source, ["source", "kind"], path);
    expectEnum(source.kind, DIAGNOSTIC_EVENT_KINDS, `${path}.kind`);
  } else if (kind === "act_token_usage") {
    expectExactFields(source, ["source"], path);
  } else {
    expectExactFields(source, ["source", "selector"], path);
    const allowed = kind === "span" ? SPAN_KINDS : kind === "instant" ? INSTANT_KINDS : COUNTER_KINDS;
    validateSelector(source.selector, `${path}.selector`, allowed);
  }
}

function validateTableColumn(value: unknown, path: string): void {
  const column = expectObject(value, path);
  const kind = expectEnum(column.column, TABLE_COLUMNS, `${path}.column`);
  if (kind === "attribute") {
    expectExactFields(column, ["column", "key"], path);
    validateCustomKey(column.key, `${path}.key`);
  } else if (kind === "token") {
    expectExactFields(column, ["column", "metric"], path);
    expectEnum(column.metric, TOKEN_METRICS, `${path}.metric`);
  } else {
    expectExactFields(column, ["column"], path);
  }
}

function validateQueryCompatibility(renderer: ViewRenderer, query: Record<string, unknown>, path: string): void {
  const source = expectObject(query.source, `${path}.source`);
  const sourceKind = expectString(source.source, `${path}.source.source`);
  const selector = source.selector === undefined ? null : expectObject(source.selector, `${path}.source.selector`);
  const selectorKind = selector?.selector;
  let outcome = sourceKind === "span" || sourceKind === "completed_span_duration";
  let severity = (sourceKind === "instant" || sourceKind === "instant_count") && selectorKind === "custom";
  let scalarFields = selectorKind === "custom" && [
    "span", "instant", "counter", "counter_value", "completed_span_duration", "instant_count",
  ].includes(sourceKind);
  let customName = selectorKind === "custom";
  let customDimensions = selectorKind === "custom" && (sourceKind === "counter" || sourceKind === "counter_value");
  if (renderer === "table" && sourceKind === "event") {
    const eventKind = source.kind;
    outcome = eventKind === "span_finished" || eventKind === "custom_span_finished";
    severity = eventKind === "custom_instant_occurred";
    scalarFields = ["custom_span_started", "custom_instant_occurred", "custom_counter_sampled"].includes(
      String(eventKind),
    );
    customName = typeof eventKind === "string" && eventKind.startsWith("custom_");
    customDimensions = eventKind === "custom_counter_sampled";
  }
  expectArray(query.filters, `${path}.filters`).forEach((raw, index) => {
    const filter = expectObject(raw, `${path}.filters[${index}]`);
    const supported = filter.filter === "outcome"
      ? outcome
      : filter.filter === "severity"
        ? severity
        : scalarFields;
    if (!supported) {
      failProtocol("filter", `${path}.filters[${index}]`, "filter is incompatible with source");
    }
  });
  if (query.group_by !== null && query.group_by !== undefined) {
    const group = expectObject(query.group_by, `${path}.group_by`);
    const dimension = group.dimension;
    const supported = ["scene", "actor", "cue", "act", "event_name"].includes(String(dimension))
      || (dimension === "custom_name" && customName)
      || (dimension === "attribute" && scalarFields)
      || (dimension === "custom_dimension" && customDimensions);
    if (!supported) {
      failProtocol("group", `${path}.group_by`, "group dimension is incompatible with source");
    }
  }
}

export function decodeViewRecord(value: unknown, path = "view"): ViewRecord {
  const record = expectObject(value, path);
  expectExactFields(
    record,
    ["renderer", "view_schema_version", "id", "title", "time_range", "scope", "query"],
    path,
  );
  const renderer = expectEnum(record.renderer, VIEW_RENDERERS, `${path}.renderer`);
  if (record.view_schema_version !== 1) {
    failProtocol("view_schema_version", `${path}.view_schema_version`, "expected integer 1");
  }
  decodeViewId(record.id, `${path}.id`);
  decodePlainTitle(record.title, `${path}.title`);
  expectEnum(record.time_range, VIEW_TIME_RANGES, `${path}.time_range`);
  expectEnum(record.scope, VIEW_SCOPES, `${path}.scope`);
  const query = expectObject(record.query, `${path}.query`);
  if (renderer === "timeline") {
    expectExactFields(query, ["source", "filters", "group_by"], `${path}.query`);
    validateTimelineSource(query.source, `${path}.query.source`);
    validateFilters(query.filters, `${path}.query.filters`);
    if (query.group_by !== null) {
      validateGroupDimension(query.group_by, `${path}.query.group_by`);
    }
  } else if (renderer === "metric" || renderer === "time_series") {
    expectExactFields(query, ["source", "filters", "group_by", "reducer"], `${path}.query`);
    const source = validateMetricSource(query.source, `${path}.query.source`);
    const reducer = expectEnum(query.reducer, VIEW_REDUCERS, `${path}.query.reducer`);
    if (source === "instant_count" && reducer !== "count") {
      failProtocol("reducer", `${path}.query.reducer`, "instant count only supports count");
    }
    validateFilters(query.filters, `${path}.query.filters`);
    if (query.group_by !== null) {
      validateGroupDimension(query.group_by, `${path}.query.group_by`);
    }
  } else {
    expectExactFields(query, ["source", "filters", "columns", "page_size"], `${path}.query`);
    validateTableSource(query.source, `${path}.query.source`);
    validateFilters(query.filters, `${path}.query.filters`);
    const columns = expectArray(query.columns, `${path}.query.columns`);
    if (columns.length === 0 || columns.length > 32) {
      failProtocol("columns", `${path}.query.columns`, "column count is out of bounds");
    }
    columns.forEach((column, index) => validateTableColumn(column, `${path}.query.columns[${index}]`));
    const pageSize = expectInteger(query.page_size, `${path}.query.page_size`);
    if (pageSize < 1 || pageSize > 500) {
      failProtocol("page_size", `${path}.query.page_size`, "page size is out of bounds");
    }
  }
  validateQueryCompatibility(renderer, query, `${path}.query`);
  return record as unknown as ViewRecord;
}

export function classifyArchivedViewRecord(value: unknown): ArchivedViewRecordResult {
  let version: number | null = null;
  try {
    const raw = expectObject(value, "view");
    version = typeof raw.view_schema_version === "number" && Number.isSafeInteger(raw.view_schema_version)
      ? raw.view_schema_version
      : null;
    if (version !== null && version > 1) {
      return {
        status: "incompatible",
        reason: "newer_view_schema",
        supported_view_schema_version: 1,
        record_view_schema_version: version,
        raw: value,
      };
    }
    return { status: "compatible", record: decodeViewRecord(value) };
  } catch {
    return {
      status: "incompatible",
      reason: "corrupt_record",
      supported_view_schema_version: 1,
      record_view_schema_version: version,
      raw: value,
    };
  }
}

export function decodeViewCapabilities(value: unknown, path = "capabilities"): ViewCapabilities {
  const capabilities = expectObject(value, path);
  const expected = {
    event_schema_version: 1,
    view_schema_version: 1,
    api_schema_version: 1,
    max_page_rows: 500,
    max_metric_series: 64,
    max_time_series_points: 1024,
    max_time_series_series: 64,
    bucket_origin: "run",
    interval_semantics: "left_closed_right_open",
    counter_selection: "latest_before_reduce",
    exact_mean_components: true,
  } as const;
  expectExactFields(capabilities, Object.keys(expected), path);
  for (const [key, expectedValue] of Object.entries(expected)) {
    if (capabilities[key] !== expectedValue) {
      failProtocol("capabilities", `${path}.${key}`, "operational capability value drifted");
    }
  }
  return capabilities as unknown as ViewCapabilities;
}

function validateBinding(value: unknown, path: string, record: ViewRecord): [bigint, bigint] {
  const binding = expectObject(value, path);
  expectExactFields(binding, [
    "captured_watermark",
    "captured_elapsed_end_ns",
    "time_range",
    "range_start_ns",
    "range_end_ns",
    "scope",
    "selected_scope",
  ], path);
  decodeU64(binding.captured_watermark, `${path}.captured_watermark`);
  const capturedEnd = BigInt(decodeU64(binding.captured_elapsed_end_ns, `${path}.captured_elapsed_end_ns`));
  const mode = expectEnum(binding.time_range, VIEW_TIME_RANGES, `${path}.time_range`);
  const start = BigInt(decodeU64(binding.range_start_ns, `${path}.range_start_ns`));
  const end = BigInt(decodeU64(binding.range_end_ns, `${path}.range_end_ns`));
  const scope = expectEnum(binding.scope, VIEW_SCOPES, `${path}.scope`);
  if (start > end || end > capturedEnd) {
    failProtocol("binding", path, "range lies outside captured data");
  }
  if (mode === "run" && (start !== 0n || end !== capturedEnd)) {
    failProtocol("binding", path, "run range is not the captured run range");
  }
  if (mode !== record.time_range || scope !== record.scope) {
    failProtocol("binding", path, "response binding differs from descriptor");
  }
  if (binding.selected_scope !== null) {
    const selected = decodeDiagnosticScope(binding.selected_scope, `${path}.selected_scope`);
    if (
      selected.effect_id !== null
      || selected.tool_call_id !== null
      || [selected.scene_id, selected.actor_id, selected.cue_id, selected.act_id].every((item) => item === null)
    ) {
      failProtocol("binding", `${path}.selected_scope`, "selection is not a domain scope");
    }
  }
  if (scope === "run" && binding.selected_scope !== null) {
    failProtocol("binding", `${path}.selected_scope`, "run scope cannot contain selection");
  }
  return [start, end];
}

function validateCoverage(value: unknown, path: string): ViewCoverage {
  const coverage = expectObject(value, path);
  expectExactFields(
    coverage,
    ["status", "matched_count", "contributing_count", "excluded_count", "excluded", "gap_count"],
    path,
  );
  const status = expectEnum(coverage.status, ["complete", "partial", "unavailable"], `${path}.status`);
  const matched = BigInt(decodeU64(coverage.matched_count, `${path}.matched_count`));
  const contributing = BigInt(decodeU64(coverage.contributing_count, `${path}.contributing_count`));
  const excludedCount = BigInt(decodeU64(coverage.excluded_count, `${path}.excluded_count`));
  const gapCount = BigInt(decodeU64(coverage.gap_count, `${path}.gap_count`));
  const excluded = expectObject(coverage.excluded, `${path}.excluded`);
  const reasonFields = [
    "open_spans",
    "missing_values",
    "non_numeric_values",
    "unavailable_values",
    "resource_truncated",
  ] as const;
  expectExactFields(excluded, reasonFields, `${path}.excluded`);
  const reasonTotal = reasonFields.reduce(
    (total, field) => total + BigInt(decodeU64(excluded[field], `${path}.excluded.${field}`)),
    0n,
  );
  if (contributing + excludedCount !== matched || reasonTotal !== excludedCount) {
    failProtocol("coverage", path, "coverage counts are inconsistent");
  }
  const complete = excludedCount === 0n && gapCount === 0n;
  if ((status === "complete" && !complete) || (status === "partial" && complete)) {
    failProtocol("coverage", path, "status disagrees with exclusions and gaps");
  }
  if (status === "unavailable" && contributing !== 0n) {
    failProtocol("coverage", path, "unavailable coverage has contributing values");
  }
  return coverage as unknown as ViewCoverage;
}

function validatePagination(value: unknown, path: string): Record<string, unknown> | null {
  if (value === null) {
    return null;
  }
  const pagination = expectObject(value, path);
  expectExactFields(pagination, ["page_size", "next_cursor"], path);
  const pageSize = expectInteger(pagination.page_size, `${path}.page_size`);
  if (pageSize < 1 || pageSize > 500) {
    failProtocol("page_size", `${path}.page_size`, "page size is out of bounds");
  }
  if (pagination.next_cursor !== null) {
    const cursor = expectString(pagination.next_cursor, `${path}.next_cursor`);
    if (cursor.length === 0 || cursor.length > 512 || !/^[\x00-\x7f]+$/.test(cursor)) {
      failProtocol("cursor", `${path}.next_cursor`, "opaque cursor is out of bounds");
    }
  }
  return pagination;
}

function validateExactNumber(value: unknown, path: string): Record<string, unknown> {
  const number = expectObject(value, path);
  expectExactFields(number, ["type", "value"], path);
  const kind = expectEnum(number.type, ["integer", "decimal"], `${path}.type`);
  (kind === "integer" ? decodeCanonicalInteger : decodeDecimal)(number.value, `${path}.value`);
  return number;
}

function validateAggregate(value: unknown, path: string): "exact" | "mean" {
  const aggregate = expectObject(value, path);
  const kind = expectEnum(aggregate.aggregate, ["exact", "mean"], `${path}.aggregate`);
  if (kind === "exact") {
    expectExactFields(aggregate, ["aggregate", "value"], path);
    validateExactNumber(aggregate.value, `${path}.value`);
  } else {
    expectExactFields(aggregate, ["aggregate", "numerator", "contributing_count"], path);
    validateExactNumber(aggregate.numerator, `${path}.numerator`);
    decodeTokenInteger(aggregate.contributing_count, `${path}.contributing_count`);
  }
  return kind;
}

function validateGroupKey(value: unknown, path: string): Record<string, unknown> | null {
  if (value === null) {
    return null;
  }
  const group = expectObject(value, path);
  expectExactFields(group, ["dimension", "value"], path);
  validateGroupDimension(group.dimension, `${path}.dimension`);
  validateTaggedScalar(group.value, `${path}.value`);
  const dimension = expectObject(group.dimension, `${path}.dimension`).dimension;
  const tagged = expectObject(group.value, `${path}.value`);
  if (["scene", "actor", "cue", "act", "event_name", "custom_name"].includes(String(dimension))) {
    if (tagged.type !== "string") {
      failProtocol("group", `${path}.value`, "built-in group value is not a string");
    }
  } else if (dimension === "custom_dimension" && tagged.type === "null") {
    failProtocol("group", `${path}.value`, "custom dimension group value is null");
  }
  return group;
}

function expectedGroup(record: ViewRecord): unknown {
  return (record.query as Record<string, unknown>).group_by;
}

function sameJson(left: unknown, right: unknown): boolean {
  return JSON.stringify(left) === JSON.stringify(right);
}

function validateGroupMatches(group: Record<string, unknown> | null, record: ViewRecord, path: string): void {
  const dimension = group === null ? null : group.dimension;
  if (!sameJson(dimension, expectedGroup(record))) {
    failProtocol("group", path, "group key differs from descriptor");
  }
}

function scopeContains(parent: DiagnosticScope, child: DiagnosticScope): boolean {
  return (Object.keys(parent) as (keyof DiagnosticScope)[]).every(
    (field) => parent[field] === null || parent[field] === child[field],
  );
}

function validateResponseCommon(
  response: Record<string, unknown>,
  path: string,
  record: ViewRecord,
): { start: bigint; end: bigint; pagination: Record<string, unknown> | null } {
  if (response.api_schema_version !== 1) {
    failProtocol("api_schema_version", `${path}.api_schema_version`, "expected integer 1");
  }
  if (response.view_schema_version !== 1) {
    failProtocol("view_schema_version", `${path}.view_schema_version`, "expected integer 1");
  }
  decodeCanonicalUuid(response.run_id, `${path}.run_id`);
  if (decodeViewId(response.view_id, `${path}.view_id`) !== record.id) {
    failProtocol("view_id", `${path}.view_id`, "response does not identify its descriptor");
  }
  const [start, end] = validateBinding(response.binding, `${path}.binding`, record);
  const coverage = validateCoverage(response.coverage, `${path}.coverage`);
  const pagination = validatePagination(response.pagination, `${path}.pagination`);
  const truncated = expectBoolean(response.truncated, `${path}.truncated`);
  if (truncated !== (coverage.excluded.resource_truncated !== "0")) {
    failProtocol("truncation", path, "truncation state and coverage disagree");
  }
  if (response.incompatible !== null) {
    const state = expectObject(response.incompatible, `${path}.incompatible`);
    expectExactFields(
      state,
      ["reason", "supported_view_schema_version", "record_view_schema_version"],
      `${path}.incompatible`,
    );
    const reason = expectEnum(
      state.reason,
      ["newer_view_schema", "corrupt_record"],
      `${path}.incompatible.reason`,
    );
    if (state.supported_view_schema_version !== 1) {
      failProtocol("view_schema_version", `${path}.incompatible`, "wrong supported version");
    }
    const version = state.record_view_schema_version;
    if (version !== null && (typeof version !== "number" || !Number.isSafeInteger(version))) {
      failProtocol("view_schema_version", `${path}.incompatible`, "record version is not an integer");
    }
    if (reason === "newer_view_schema" && (typeof version !== "number" || version <= 1)) {
      failProtocol("view_schema_version", `${path}.incompatible`, "newer reason is not newer");
    }
    if (reason === "corrupt_record" && typeof version === "number" && version > 1) {
      failProtocol("view_schema_version", `${path}.incompatible`, "newer record is not corrupt");
    }
  }
  decodeViewCapabilities(response.capabilities, `${path}.capabilities`);
  return { start, end, pagination };
}

function validateAggregateForQuery(
  value: unknown,
  coverage: ViewCoverage,
  record: ViewRecord,
  path: string,
): void {
  if (value === null) {
    if (coverage.contributing_count !== "0") {
      failProtocol("coverage", path, "empty aggregate has contributing values");
    }
    return;
  }
  const kind = validateAggregate(value, `${path}.value`);
  const query = record.query as Record<string, unknown>;
  const reducer = query.reducer;
  if (kind !== (reducer === "mean" ? "mean" : "exact")) {
    failProtocol("reducer", `${path}.value`, "aggregate shape differs from reducer");
  }
  const aggregate = expectObject(value, `${path}.value`);
  const exact = expectObject(kind === "mean" ? aggregate.numerator : aggregate.value, `${path}.value.number`);
  if (reducer === "count" && (exact.type !== "integer" || String(exact.value).startsWith("-"))) {
    failProtocol("reducer", `${path}.value`, "count is not a nonnegative integer");
  }
  const source = expectObject(query.source, "view.query.source").source;
  if (
    (source === "completed_span_duration" || source === "act_token")
    && (exact.type !== "integer" || String(exact.value).startsWith("-"))
  ) {
    failProtocol("source", `${path}.value`, "integral source has non-integral value");
  }
  if (kind === "mean" && aggregate.contributing_count !== coverage.contributing_count) {
    failProtocol("coverage", path, "mean count differs from contributing coverage");
  }
}

export function decodeViewResponse(
  value: unknown,
  record: ViewRecord,
  path = "response",
): ViewResponse {
  const response = expectObject(value, path);
  const renderer = expectEnum(response.renderer, VIEW_RENDERERS, `${path}.renderer`);
  if (renderer !== record.renderer) {
    failProtocol("renderer", `${path}.renderer`, "response renderer differs from descriptor");
  }
  const common = [
    "renderer",
    "api_schema_version",
    "view_schema_version",
    "run_id",
    "view_id",
    "binding",
    "coverage",
    "pagination",
    "truncated",
    "incompatible",
    "capabilities",
  ];
  const dataFields: Record<ViewRenderer, readonly string[]> = {
    timeline: ["rows"],
    metric: ["series"],
    table: ["columns", "rows"],
    time_series: ["bucket_width_ns", "series"],
  };
  expectExactFields(response, [...common, ...dataFields[renderer]], path);
  const { start, end, pagination } = validateResponseCommon(response, path, record);
  const incompatible = response.incompatible !== null;
  const responseCoverage = response.coverage as ViewCoverage;
  if (incompatible && responseCoverage.status !== "unavailable") {
    failProtocol("incompatible", path, "incompatible result coverage is not unavailable");
  }
  if (renderer === "timeline" || renderer === "table") {
    if (pagination === null) {
      failProtocol("pagination", `${path}.pagination`, "row renderer requires pagination");
    }
  } else if (pagination !== null) {
    failProtocol("pagination", `${path}.pagination`, "aggregate renderer cannot be paginated");
  }

  if (renderer === "timeline") {
    const rows = expectArray(response.rows, `${path}.rows`);
    if (incompatible && rows.length > 0) {
      failProtocol("incompatible", `${path}.rows`, "incompatible timeline contains rows");
    }
    const pageSize = pagination!.page_size as number;
    if (rows.length > pageSize) {
      failProtocol("page_size", `${path}.rows`, "timeline result exceeds page size");
    }
    const binding = response.binding as ViewBinding;
    const watermark = BigInt(binding.captured_watermark);
    const capturedEnd = BigInt(binding.captured_elapsed_end_ns);
    let previous = 0n;
    rows.forEach((raw, index) => {
      const rowPath = `${path}.rows[${index}]`;
      const row = expectObject(raw, rowPath);
      expectExactFields(
        row,
        ["sequence", "group", "item_type", "name", "start_ns", "end_ns", "scope", "outcome"],
        rowPath,
      );
      const sequence = BigInt(decodeU64(row.sequence, `${rowPath}.sequence`));
      if (sequence === 0n || sequence <= previous || sequence > watermark) {
        failProtocol("sequence", `${rowPath}.sequence`, "row lies outside captured ordered prefix");
      }
      previous = sequence;
      const group = validateGroupKey(row.group, `${rowPath}.group`);
      validateGroupMatches(group, record, `${rowPath}.group`);
      const itemType = expectEnum(row.item_type, ["span", "instant"], `${rowPath}.item_type`);
      const name = expectString(row.name, `${rowPath}.name`);
      const rowStart = BigInt(decodeU64(row.start_ns, `${rowPath}.start_ns`));
      const rowEndString = optional(row.end_ns, decodeU64, `${rowPath}.end_ns`);
      const rowEnd = rowEndString === null ? null : BigInt(rowEndString);
      const scope = decodeDiagnosticScope(row.scope, `${rowPath}.scope`);
      if (group !== null) {
        const dimension = expectObject(group.dimension, `${rowPath}.group.dimension`).dimension;
        const groupValue = expectObject(group.value, `${rowPath}.group.value`).value;
        if (["scene", "actor", "cue", "act"].includes(String(dimension))) {
          const scopeField = `${String(dimension)}_id` as keyof DiagnosticScope;
          if (groupValue !== scope[scopeField]) {
            failProtocol("group", `${rowPath}.group`, "group value differs from row scope");
          }
        } else if ((dimension === "event_name" || dimension === "custom_name") && groupValue !== name) {
          failProtocol("group", `${rowPath}.group`, "group value differs from row name");
        }
      }
      const outcome = optional(
        row.outcome,
        (item, itemPath) => expectEnum(item, SPAN_OUTCOMES, itemPath),
        `${rowPath}.outcome`,
      );
      if (itemType === "instant" && (rowEnd !== null || outcome !== null)) {
        failProtocol("timeline", rowPath, "instant contains span-only fields");
      }
      if (rowEnd !== null && rowEnd < rowStart) {
        failProtocol("timeline", rowPath, "span ends before it starts");
      }
      if (itemType === "span" && ((rowEnd === null) !== (outcome === null))) {
        failProtocol("timeline", rowPath, "span completion and outcome disagree");
      }
      if (rowStart > capturedEnd || (rowEnd !== null && rowEnd > capturedEnd)) {
        failProtocol("binding", rowPath, "timeline row lies beyond captured time");
      }
      const intersects = itemType === "instant"
        ? start <= rowStart && rowStart < end
        : start < end && rowStart < end && (rowEnd ?? capturedEnd) > start;
      if (!intersects) {
        failProtocol("binding", rowPath, "timeline row does not intersect query range");
      }
      if (binding.selected_scope !== null && !scopeContains(binding.selected_scope, scope)) {
        failProtocol("binding", `${rowPath}.scope`, "row lies outside selected scope");
      }
      const source = expectObject(record.query.source, "view.query.source");
      const selector = expectObject(source.selector, "view.query.source.selector");
      const expectedName = selector.kind ?? selector.name;
      if (itemType !== source.source || name !== expectedName) {
        failProtocol("source", rowPath, "timeline row differs from query source");
      }
    });
  } else if (renderer === "metric") {
    const series = expectArray(response.series, `${path}.series`);
    if (incompatible && series.length > 0) {
      failProtocol("incompatible", `${path}.series`, "incompatible metric contains series");
    }
    if (series.length > 64) {
      failProtocol("series_cap", `${path}.series`, "metric series count exceeds 64");
    }
    const seen = new Set<string>();
    series.forEach((raw, index) => {
      const seriesPath = `${path}.series[${index}]`;
      const item = expectObject(raw, seriesPath);
      expectExactFields(item, ["group", "value", "coverage"], seriesPath);
      const group = validateGroupKey(item.group, `${seriesPath}.group`);
      const identity = JSON.stringify(group);
      if (seen.has(identity)) {
        failProtocol("group", `${seriesPath}.group`, "duplicate metric group");
      }
      seen.add(identity);
      validateGroupMatches(group, record, `${seriesPath}.group`);
      const coverage = validateCoverage(item.coverage, `${seriesPath}.coverage`);
      validateAggregateForQuery(item.value, coverage, record, seriesPath);
    });
  } else if (renderer === "table") {
    const columns = expectArray(response.columns, `${path}.columns`);
    if (!sameJson(columns, record.query.columns)) {
      failProtocol("columns", `${path}.columns`, "response columns differ from descriptor");
    }
    columns.forEach((column, index) => validateTableColumn(column, `${path}.columns[${index}]`));
    const rows = expectArray(response.rows, `${path}.rows`);
    if (incompatible && rows.length > 0) {
      failProtocol("incompatible", `${path}.rows`, "incompatible table contains rows");
    }
    const pageSize = pagination!.page_size as number;
    if (rows.length > pageSize || rows.length > 500) {
      failProtocol("page_size", `${path}.rows`, "table result exceeds page size");
    }
    const watermark = BigInt((response.binding as ViewBinding).captured_watermark);
    let previous = 0n;
    rows.forEach((raw, index) => {
      const rowPath = `${path}.rows[${index}]`;
      const row = expectObject(raw, rowPath);
      expectExactFields(row, ["sequence", "cells"], rowPath);
      const sequence = BigInt(decodeU64(row.sequence, `${rowPath}.sequence`));
      if (sequence === 0n || sequence <= previous || sequence > watermark) {
        failProtocol("sequence", `${rowPath}.sequence`, "row lies outside captured ordered prefix");
      }
      previous = sequence;
      const cells = expectArray(row.cells, `${rowPath}.cells`);
      if (cells.length !== columns.length) {
        failProtocol("columns", `${rowPath}.cells`, "cell count differs from columns");
      }
      cells.forEach((cell, cellIndex) => {
        if (cell !== null) {
          validateTaggedScalar(cell, `${rowPath}.cells[${cellIndex}]`);
        }
      });
    });
  } else {
    const width = BigInt(decodeU64(response.bucket_width_ns, `${path}.bucket_width_ns`));
    const duration = end - start;
    const expectedWidth = duration === 0n ? 1n : ((duration + 1022n) / 1023n > 1n ? (duration + 1022n) / 1023n : 1n);
    if (width !== expectedWidth) {
      failProtocol("bucket_width", `${path}.bucket_width_ns`, `expected ${expectedWidth}`);
    }
    const expectedBuckets: [bigint, bigint, boolean][] = [];
    if (start !== end) {
      for (let bucketStart = (start / width) * width; bucketStart < end; bucketStart += width) {
        const bucketEnd = bucketStart + width;
        expectedBuckets.push([bucketStart, bucketEnd, bucketStart < start || bucketEnd > end]);
      }
    }
    if (expectedBuckets.length > 1024) {
      failProtocol("point_cap", path, "more than 1024 origin-aligned buckets");
    }
    const series = expectArray(response.series, `${path}.series`);
    if (incompatible && series.length > 0) {
      failProtocol("incompatible", `${path}.series`, "incompatible time-series contains series");
    }
    if (series.length > 64) {
      failProtocol("series_cap", `${path}.series`, "time-series count exceeds 64");
    }
    const seen = new Set<string>();
    series.forEach((raw, seriesIndex) => {
      const seriesPath = `${path}.series[${seriesIndex}]`;
      const item = expectObject(raw, seriesPath);
      expectExactFields(item, ["group", "points"], seriesPath);
      const group = validateGroupKey(item.group, `${seriesPath}.group`);
      const identity = JSON.stringify(group);
      if (seen.has(identity)) {
        failProtocol("group", `${seriesPath}.group`, "duplicate time-series group");
      }
      seen.add(identity);
      validateGroupMatches(group, record, `${seriesPath}.group`);
      const points = expectArray(item.points, `${seriesPath}.points`);
      if (points.length !== expectedBuckets.length) {
        failProtocol("buckets", `${seriesPath}.points`, "empty or intersecting bucket is missing");
      }
      points.forEach((rawPoint, pointIndex) => {
        const pointPath = `${seriesPath}.points[${pointIndex}]`;
        const point = expectObject(rawPoint, pointPath);
        expectExactFields(
          point,
          ["bucket_start_ns", "bucket_end_ns", "partial", "value", "coverage"],
          pointPath,
        );
        const actual: [bigint, bigint, boolean] = [
          BigInt(decodeU64(point.bucket_start_ns, `${pointPath}.bucket_start_ns`)),
          BigInt(decodeU64(point.bucket_end_ns, `${pointPath}.bucket_end_ns`)),
          expectBoolean(point.partial, `${pointPath}.partial`),
        ];
        const expected = expectedBuckets[pointIndex]!;
        if (actual[0] !== expected[0] || actual[1] !== expected[1] || actual[2] !== expected[2]) {
          failProtocol("buckets", pointPath, "bucket is not origin aligned");
        }
        const coverage = validateCoverage(point.coverage, `${pointPath}.coverage`);
        validateAggregateForQuery(point.value, coverage, record, pointPath);
      });
    });
  }
  return response as unknown as ViewResponse;
}

export function encodeViewRecordJson(record: ViewRecord): string {
  return JSON.stringify(record);
}

export function encodeViewResponseJson(response: ViewResponse): string {
  return JSON.stringify(response);
}
