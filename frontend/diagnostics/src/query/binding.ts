import {
  type CanonicalUuid,
  type U64String,
  decodeCanonicalUuid,
  decodeU64,
  expectEnum,
  expectExactFields,
  expectObject,
  expectString,
  failProtocol,
} from "../protocol/decimal.ts";
import {
  type DiagnosticScope,
  decodeDiagnosticScope,
} from "../protocol/event.ts";
import type {
  TimeSeriesViewResponse,
  ViewBinding,
  ViewRecord,
  ViewResponse,
} from "../protocol/view.ts";
import type { SelectionReference } from "../state/model.ts";


const SCOPE_FIELDS = [
  "scene_id",
  "actor_id",
  "cue_id",
  "effect_id",
  "act_id",
  "tool_call_id",
  "session_generation",
] as const;

const QUERY_SCOPE_FIELDS = [
  "scene_id",
  "actor_id",
  "cue_id",
  "act_id",
  "session_generation",
] as const;

export interface ViewportBinding {
  readonly start_ns: U64String;
  readonly end_ns: U64String;
}

export interface ViewQueryContext {
  readonly captured_watermark: U64String;
  readonly captured_elapsed_end_ns: U64String;
  readonly selection: SelectionReference | null;
  readonly selected_scope: DiagnosticScope | null;
  readonly viewport: ViewportBinding | null;
}

export interface ViewQueryGeneration {
  readonly key: string;
  readonly run_id: CanonicalUuid;
  readonly view_id: string;
  readonly renderer: ViewRecord["renderer"];
  readonly selection: SelectionReference | null;
  readonly selected_scope: DiagnosticScope | null;
  readonly viewport: ViewportBinding | null;
  readonly binding: ViewBinding;
  readonly expected_bucket_width_ns: U64String | null;
}

function freezeSelection(value: SelectionReference | null): SelectionReference | null {
  if (value === null) {
    return null;
  }
  const selection = expectObject(value, "query.selection");
  expectExactFields(selection, ["kind", "id"], "query.selection");
  const kind = expectEnum(
    selection.kind,
    ["event", "span", "message", "scope"],
    "query.selection.kind",
  );
  const id = expectString(selection.id, "query.selection.id");
  if (id.length === 0) {
    failProtocol("selection", "query.selection.id", "selection identity is empty");
  }
  return { kind, id };
}

function freezeDomainScope(value: DiagnosticScope | null): DiagnosticScope | null {
  if (value === null) {
    return null;
  }
  const decoded = decodeDiagnosticScope(value, "query.selected_scope");
  if (
    decoded.effect_id !== null
    || decoded.tool_call_id !== null
    || [decoded.scene_id, decoded.actor_id, decoded.cue_id, decoded.act_id]
      .every((field) => field === null)
  ) {
    failProtocol(
      "binding",
      "query.selected_scope",
      "selection queries require a Scene/Actor/Cue/Act domain scope",
    );
  }
  return {
    scene_id: decoded.scene_id,
    actor_id: decoded.actor_id,
    cue_id: decoded.cue_id,
    effect_id: null,
    act_id: decoded.act_id,
    tool_call_id: null,
    session_generation: decoded.session_generation,
  };
}

function freezeViewport(
  value: ViewportBinding | null,
  capturedEnd: U64String,
  requireCapturedRange: boolean,
): ViewportBinding | null {
  if (value === null) {
    return null;
  }
  const viewport = expectObject(value, "query.viewport");
  expectExactFields(viewport, ["start_ns", "end_ns"], "query.viewport");
  const start = decodeU64(viewport.start_ns, "query.viewport.start_ns");
  const end = decodeU64(viewport.end_ns, "query.viewport.end_ns");
  if (
    BigInt(start) > BigInt(end)
    || (requireCapturedRange && BigInt(end) > BigInt(capturedEnd))
  ) {
    failProtocol("binding", "query.viewport", "viewport lies outside captured data");
  }
  return { start_ns: start, end_ns: end };
}

export function expectedTimeSeriesBucketWidth(binding: ViewBinding): U64String {
  const start = BigInt(binding.range_start_ns);
  const end = BigInt(binding.range_end_ns);
  const duration = end - start;
  const width = duration === 0n ? 1n : (duration + 1022n) / 1023n;
  return decodeU64((width > 1n ? width : 1n).toString(), "query.bucket_width_ns");
}

export function freezeViewQueryGeneration(
  runId: CanonicalUuid,
  record: ViewRecord,
  context: ViewQueryContext,
): ViewQueryGeneration {
  const run = decodeCanonicalUuid(runId, "query.run_id");
  const capturedWatermark = decodeU64(
    context.captured_watermark,
    "query.captured_watermark",
  );
  const capturedEnd = decodeU64(
    context.captured_elapsed_end_ns,
    "query.captured_elapsed_end_ns",
  );
  const selection = freezeSelection(context.selection);
  const selectedScope = freezeDomainScope(context.selected_scope);
  const viewport = freezeViewport(
    context.viewport,
    capturedEnd,
    record.time_range === "viewport",
  );
  if (record.time_range === "viewport" && viewport === null) {
    failProtocol("binding", "query.viewport", "viewport-bound view requires a viewport");
  }

  const rangeStart = record.time_range === "run"
    ? decodeU64("0")
    : viewport!.start_ns;
  const rangeEnd = record.time_range === "run"
    ? capturedEnd
    : viewport!.end_ns;
  const binding: ViewBinding = {
    captured_watermark: capturedWatermark,
    captured_elapsed_end_ns: capturedEnd,
    time_range: record.time_range,
    range_start_ns: rangeStart,
    range_end_ns: rangeEnd,
    scope: record.scope,
    selected_scope: record.scope === "selection" ? selectedScope : null,
  };
  const expectedBucketWidth = record.renderer === "time_series"
    ? expectedTimeSeriesBucketWidth(binding)
    : null;
  const key = JSON.stringify([
    "troupe-view-query-generation-v1",
    run,
    record.id,
    record.renderer,
    record.time_range,
    record.scope,
    selection === null ? null : [selection.kind, selection.id],
    selectedScope === null ? null : SCOPE_FIELDS.map((field) => selectedScope[field]),
    capturedWatermark,
    capturedEnd,
    viewport === null ? null : [viewport.start_ns, viewport.end_ns],
  ]);
  return {
    key,
    run_id: run,
    view_id: record.id,
    renderer: record.renderer,
    selection,
    selected_scope: selectedScope,
    viewport,
    binding,
    expected_bucket_width_ns: expectedBucketWidth,
  };
}

export function appendViewBindingParameters(
  parameters: URLSearchParams,
  generation: ViewQueryGeneration,
): void {
  parameters.set("view_id", generation.view_id);
  const binding = generation.binding;
  if (binding.time_range === "viewport") {
    parameters.set("viewport_start_ns", binding.range_start_ns);
    parameters.set("viewport_end_ns", binding.range_end_ns);
  }
  if (binding.scope === "selection" && binding.selected_scope !== null) {
    for (const field of QUERY_SCOPE_FIELDS) {
      const value = binding.selected_scope[field];
      if (value !== null) {
        parameters.set(field, value);
      }
    }
  }
  parameters.set("captured_watermark", binding.captured_watermark);
  parameters.set("captured_elapsed_end_ns", binding.captured_elapsed_end_ns);
}

function sameScope(left: DiagnosticScope | null, right: DiagnosticScope | null): boolean {
  return left === null || right === null
    ? left === right
    : SCOPE_FIELDS.every((field) => left[field] === right[field]);
}

export function assertViewResponseGeneration(
  response: ViewResponse,
  generation: ViewQueryGeneration,
): void {
  if (
    response.run_id !== generation.run_id
    || response.view_id !== generation.view_id
    || response.renderer !== generation.renderer
  ) {
    failProtocol("binding", "response", "response identity differs from query generation");
  }
  const expected = generation.binding;
  const actual = response.binding;
  if (
    actual.captured_watermark !== expected.captured_watermark
    || actual.captured_elapsed_end_ns !== expected.captured_elapsed_end_ns
    || actual.time_range !== expected.time_range
    || actual.range_start_ns !== expected.range_start_ns
    || actual.range_end_ns !== expected.range_end_ns
    || actual.scope !== expected.scope
    || !sameScope(actual.selected_scope, expected.selected_scope)
  ) {
    failProtocol("binding", "response.binding", "response binding is not the frozen request binding");
  }
  if (
    response.renderer === "time_series"
    && response.bucket_width_ns !== generation.expected_bucket_width_ns
  ) {
    failProtocol(
      "bucket_width",
      "response.bucket_width_ns",
      "response width is not derived from the frozen range",
    );
  }
}

export function isTimeSeriesResponse(
  response: ViewResponse,
): response is TimeSeriesViewResponse {
  return response.renderer === "time_series";
}
