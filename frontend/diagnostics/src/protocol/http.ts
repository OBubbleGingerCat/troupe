import {
  type CanonicalUuid,
  type JsonObject,
  type TokenIntegerString,
  type U64String,
  decodeCanonicalUuid,
  decodeJsonValue,
  decodeTokenInteger,
  decodeU64,
  expectArray,
  expectEnum,
  expectExactFields,
  expectObject,
  expectString,
  failProtocol,
} from "./decimal.ts";
import {
  type ActTokenUsageFinalizedEvent,
  type DiagnosticEvent,
  type DiagnosticScope,
  type ObservationGapEvent,
  decodeDiagnosticScope,
  decodeDiagnosticEvent,
  decodeRunLocalId,
} from "./event.ts";


export interface VersionedApiObject {
  readonly api_schema_version: number;
  readonly run_id: CanonicalUuid;
  readonly [key: string]: unknown;
}

export interface SnapshotResponse {
  readonly api_schema_version: 1;
  readonly run_id: CanonicalUuid;
  readonly watermark_sequence: U64String;
  readonly earliest_available_sequence: U64String | null;
  readonly state: SnapshotState;
}

export interface SnapshotState {
  readonly model_schema_version: 1;
  readonly run_id: CanonicalUuid;
  readonly through_sequence: U64String;
  readonly through_elapsed_ns: U64String;
  readonly spans: JsonObject;
  readonly messages: JsonObject;
  readonly plans: JsonObject;
  readonly counters: JsonObject;
  readonly usage: UsageSnapshot;
  readonly gaps: readonly ObservationGapEvent[];
  readonly truncations: readonly SnapshotTruncation[];
}

export type SnapshotTruncation =
  | {
    readonly source: "agent_message";
    readonly sequence: U64String;
    readonly scope: DiagnosticScope;
    readonly message_id: string;
  }
  | {
    readonly source: "agent_plan";
    readonly sequence: U64String;
    readonly scope: DiagnosticScope;
  };

export interface UsageFieldAggregateSnapshot {
  readonly known_sum: TokenIntegerString | null;
  readonly reported_acts: U64String;
  readonly finalized_acts: U64String;
}

export interface UsageAggregateSnapshot {
  readonly finalized_acts: U64String;
  readonly reported_acts: U64String;
  readonly available_acts: U64String;
  readonly partial_acts: U64String;
  readonly unavailable_acts: U64String;
  readonly provider_total_tokens: UsageFieldAggregateSnapshot;
  readonly input_tokens: UsageFieldAggregateSnapshot;
  readonly output_tokens: UsageFieldAggregateSnapshot;
  readonly thought_tokens: UsageFieldAggregateSnapshot;
  readonly cached_read_tokens: UsageFieldAggregateSnapshot;
  readonly cached_write_tokens: UsageFieldAggregateSnapshot;
}

export interface ProjectedActUsageSnapshot {
  readonly act_id: string;
  readonly event: ActTokenUsageFinalizedEvent;
}

export interface ScopedUsageAggregateSnapshot {
  readonly scope: DiagnosticScope;
  readonly aggregate: UsageAggregateSnapshot;
}

export interface UsageSnapshot {
  readonly model_schema_version: 1;
  readonly run_id: CanonicalUuid;
  readonly through_sequence: U64String;
  readonly through_elapsed_ns: U64String;
  readonly usages: readonly ProjectedActUsageSnapshot[];
  readonly aggregate: UsageAggregateSnapshot;
  readonly scoped_aggregates: readonly ScopedUsageAggregateSnapshot[];
}

export interface EventsResponse {
  readonly api_schema_version: 1;
  readonly run_id: CanonicalUuid;
  readonly captured_watermark: U64String;
  readonly events: readonly DiagnosticEvent[];
  readonly next_after: U64String | null;
}

export interface HttpErrorResponse {
  readonly api_schema_version: 1;
  readonly run_id: CanonicalUuid | null;
  readonly error: {
    readonly code: string;
    readonly message: string;
    readonly details: JsonObject | null;
  };
}

function requireApiVersion(value: unknown, path: string): void {
  if (value !== 1) {
    failProtocol("api_schema_version", path, "expected integer 1");
  }
}

const USAGE_FIELDS = [
  "provider_total_tokens",
  "input_tokens",
  "output_tokens",
  "thought_tokens",
  "cached_read_tokens",
  "cached_write_tokens",
] as const;

interface UsageAggregateAccumulator {
  finalized_acts: bigint;
  reported_acts: bigint;
  available_acts: bigint;
  partial_acts: bigint;
  unavailable_acts: bigint;
  fields: Record<typeof USAGE_FIELDS[number], {
    known_sum: bigint;
    reported_acts: bigint;
  }>;
}

function emptyUsageAggregateAccumulator(): UsageAggregateAccumulator {
  return {
    finalized_acts: 0n,
    reported_acts: 0n,
    available_acts: 0n,
    partial_acts: 0n,
    unavailable_acts: 0n,
    fields: Object.fromEntries(USAGE_FIELDS.map((field) => [
      field,
      { known_sum: 0n, reported_acts: 0n },
    ])) as UsageAggregateAccumulator["fields"],
  };
}

function recordUsage(
  accumulator: UsageAggregateAccumulator,
  usage: ProjectedActUsageSnapshot,
): void {
  accumulator.finalized_acts += 1n;
  if (usage.event.availability === "available") {
    accumulator.reported_acts += 1n;
    accumulator.available_acts += 1n;
  } else if (usage.event.availability === "partial") {
    accumulator.reported_acts += 1n;
    accumulator.partial_acts += 1n;
  } else {
    accumulator.unavailable_acts += 1n;
  }
  for (const field of USAGE_FIELDS) {
    const value = usage.event[field];
    if (value !== null) {
      accumulator.fields[field].known_sum += BigInt(value);
      accumulator.fields[field].reported_acts += 1n;
    }
  }
}

function finishUsageAggregate(
  accumulator: UsageAggregateAccumulator,
): UsageAggregateSnapshot {
  const finalized = decodeU64(String(accumulator.finalized_acts));
  const fields = Object.fromEntries(USAGE_FIELDS.map((field) => {
    const value = accumulator.fields[field];
    return [field, {
      known_sum: value.reported_acts === 0n
        ? null
        : decodeTokenInteger(String(value.known_sum)),
      reported_acts: decodeU64(String(value.reported_acts)),
      finalized_acts: finalized,
    }];
  })) as unknown as Pick<UsageAggregateSnapshot, typeof USAGE_FIELDS[number]>;
  return {
    finalized_acts: finalized,
    reported_acts: decodeU64(String(accumulator.reported_acts)),
    available_acts: decodeU64(String(accumulator.available_acts)),
    partial_acts: decodeU64(String(accumulator.partial_acts)),
    unavailable_acts: decodeU64(String(accumulator.unavailable_acts)),
    ...fields,
  };
}

function aggregateUsages(usages: readonly ProjectedActUsageSnapshot[]): UsageAggregateSnapshot {
  const accumulator = emptyUsageAggregateAccumulator();
  for (const usage of usages) {
    recordUsage(accumulator, usage);
  }
  return finishUsageAggregate(accumulator);
}

function sameUsageAggregate(
  left: UsageAggregateSnapshot,
  right: UsageAggregateSnapshot,
): boolean {
  return JSON.stringify(left) === JSON.stringify(right);
}

function aggregateScope(sceneId: string, actorId: string | null): DiagnosticScope {
  return {
    scene_id: sceneId,
    actor_id: actorId,
    cue_id: null,
    effect_id: null,
    act_id: null,
    tool_call_id: null,
    session_generation: null,
  };
}

function scopeIdentity(scope: DiagnosticScope): string {
  return JSON.stringify([scope.scene_id, scope.actor_id]);
}

function aggregateUsagesByScope(
  usages: readonly ProjectedActUsageSnapshot[],
): readonly ScopedUsageAggregateSnapshot[] {
  const ordered = new Map<string, {
    scope: DiagnosticScope;
    accumulator: UsageAggregateAccumulator;
  }>();
  for (const usage of usages) {
    const sceneId = usage.event.scope.scene_id;
    if (sceneId === null) {
      continue;
    }
    const scopes = [aggregateScope(sceneId, null)];
    if (usage.event.scope.actor_id !== null) {
      scopes.push(aggregateScope(sceneId, usage.event.scope.actor_id));
    }
    for (const scope of scopes) {
      const identity = scopeIdentity(scope);
      let entry = ordered.get(identity);
      if (entry === undefined) {
        entry = { scope, accumulator: emptyUsageAggregateAccumulator() };
        ordered.set(identity, entry);
      }
      recordUsage(entry.accumulator, usage);
    }
  }
  return [...ordered.values()].map((entry) => ({
    scope: entry.scope,
    aggregate: finishUsageAggregate(entry.accumulator),
  }));
}

function decodeUsageFieldAggregate(
  value: unknown,
  path: string,
): UsageFieldAggregateSnapshot {
  const field = expectObject(value, path);
  expectExactFields(field, ["known_sum", "reported_acts", "finalized_acts"], path);
  const knownSum = field.known_sum === null
    ? null
    : decodeTokenInteger(field.known_sum, `${path}.known_sum`);
  const reported = decodeU64(field.reported_acts, `${path}.reported_acts`);
  const finalized = decodeU64(field.finalized_acts, `${path}.finalized_acts`);
  if (BigInt(reported) > BigInt(finalized) || (knownSum === null) !== (reported === "0")) {
    failProtocol("usage_coverage", path, "known sum and coverage are inconsistent");
  }
  return { known_sum: knownSum, reported_acts: reported, finalized_acts: finalized };
}

function decodeUsageAggregate(value: unknown, path: string): UsageAggregateSnapshot {
  const aggregate = expectObject(value, path);
  expectExactFields(
    aggregate,
    [
      "finalized_acts",
      "reported_acts",
      "available_acts",
      "partial_acts",
      "unavailable_acts",
      ...USAGE_FIELDS,
    ],
    path,
  );
  const finalized = decodeU64(aggregate.finalized_acts, `${path}.finalized_acts`);
  const reported = decodeU64(aggregate.reported_acts, `${path}.reported_acts`);
  const available = decodeU64(aggregate.available_acts, `${path}.available_acts`);
  const partial = decodeU64(aggregate.partial_acts, `${path}.partial_acts`);
  const unavailable = decodeU64(aggregate.unavailable_acts, `${path}.unavailable_acts`);
  if (
    BigInt(available) + BigInt(partial) + BigInt(unavailable) !== BigInt(finalized)
    || BigInt(available) + BigInt(partial) !== BigInt(reported)
  ) {
    failProtocol("usage_coverage", path, "availability counts are inconsistent");
  }
  const fields = Object.fromEntries(USAGE_FIELDS.map((field) => [
    field,
    decodeUsageFieldAggregate(aggregate[field], `${path}.${field}`),
  ])) as unknown as Pick<UsageAggregateSnapshot, typeof USAGE_FIELDS[number]>;
  for (const field of USAGE_FIELDS) {
    if (fields[field].finalized_acts !== finalized) {
      failProtocol("usage_coverage", `${path}.${field}`, "field coverage has another denominator");
    }
  }
  return {
    finalized_acts: finalized,
    reported_acts: reported,
    available_acts: available,
    partial_acts: partial,
    unavailable_acts: unavailable,
    ...fields,
  };
}

function decodeProjectedActUsage(value: unknown, path: string): ProjectedActUsageSnapshot {
  const projected = expectObject(value, path);
  expectExactFields(
    projected,
    [
      "run_id",
      "act_id",
      "scope",
      "sequence",
      "elapsed_ns",
      "caused_by",
      "availability",
      "source",
      "unavailable_reason",
      ...USAGE_FIELDS,
    ],
    path,
  );
  const actId = expectString(projected.act_id, `${path}.act_id`);
  const { act_id: _actId, ...eventFields } = projected;
  const event = decodeDiagnosticEvent({
    schema_version: 1,
    kind: "act_token_usage_finalized",
    ...eventFields,
  }, path);
  if (event.kind !== "act_token_usage_finalized" || event.scope.act_id !== actId) {
    failProtocol("usage_identity", path, "projected Act identity does not match its scope");
  }
  return { act_id: actId, event };
}

function decodeScopedUsageAggregate(
  value: unknown,
  path: string,
): ScopedUsageAggregateSnapshot {
  const scoped = expectObject(value, path);
  expectExactFields(scoped, ["scope", "aggregate"], path);
  const scope = decodeDiagnosticScope(scoped.scope, `${path}.scope`);
  if (
    scope.scene_id === null
    || scope.cue_id !== null
    || scope.effect_id !== null
    || scope.act_id !== null
    || scope.tool_call_id !== null
    || scope.session_generation !== null
  ) {
    failProtocol("usage_scope", `${path}.scope`, "expected a Scene or Actor aggregate scope");
  }
  return {
    scope,
    aggregate: decodeUsageAggregate(scoped.aggregate, `${path}.aggregate`),
  };
}

export function decodeUsageSnapshot(
  value: unknown,
  expectedRunId: CanonicalUuid,
  expectedThrough: U64String,
  expectedElapsed: U64String,
  path = "snapshot.state.usage",
): UsageSnapshot {
  const usage = expectObject(value, path);
  expectExactFields(
    usage,
    [
      "model_schema_version",
      "run_id",
      "through_sequence",
      "through_elapsed_ns",
      "usages",
      "aggregate",
      "scoped_aggregates",
    ],
    path,
  );
  requireApiVersion(usage.model_schema_version, `${path}.model_schema_version`);
  const runId = decodeCanonicalUuid(usage.run_id, `${path}.run_id`);
  const through = decodeU64(usage.through_sequence, `${path}.through_sequence`);
  const elapsed = decodeU64(usage.through_elapsed_ns, `${path}.through_elapsed_ns`);
  if (runId !== expectedRunId || through !== expectedThrough || elapsed !== expectedElapsed) {
    failProtocol("usage_identity", path, "usage snapshot differs from its response envelope");
  }
  const usages = expectArray(usage.usages, `${path}.usages`).map((item, index) => (
    decodeProjectedActUsage(item, `${path}.usages[${index}]`)
  ));
  let previous = 0n;
  const actIds = new Set<string>();
  for (const item of usages) {
    const sequence = BigInt(item.event.sequence);
    if (
      item.event.run_id !== runId
      || sequence <= previous
      || sequence > BigInt(through)
      || BigInt(item.event.elapsed_ns) > BigInt(elapsed)
      || actIds.has(item.act_id)
    ) {
      failProtocol("usage_identity", `${path}.usages`, "projected usages are not a unique ordered prefix");
    }
    previous = sequence;
    actIds.add(item.act_id);
  }
  const aggregate = decodeUsageAggregate(usage.aggregate, `${path}.aggregate`);
  if (!sameUsageAggregate(aggregate, aggregateUsages(usages))) {
    failProtocol("usage_aggregate", `${path}.aggregate`, "aggregate does not match terminal usage facts");
  }
  const scopedAggregates = expectArray(usage.scoped_aggregates, `${path}.scoped_aggregates`)
    .map((item, index) => (
      decodeScopedUsageAggregate(item, `${path}.scoped_aggregates[${index}]`)
    ));
  const expectedScoped = aggregateUsagesByScope(usages);
  if (scopedAggregates.length !== expectedScoped.length) {
    failProtocol(
      "usage_scope",
      `${path}.scoped_aggregates`,
      "scoped aggregate inventory does not match terminal usage facts",
    );
  }
  for (const [index, item] of scopedAggregates.entries()) {
    const expected = expectedScoped[index]!;
    if (
      scopeIdentity(item.scope) !== scopeIdentity(expected.scope)
      || !sameUsageAggregate(item.aggregate, expected.aggregate)
    ) {
      failProtocol(
        "usage_scope",
        `${path}.scoped_aggregates[${index}]`,
        "scoped aggregate order or value does not match terminal usage facts",
      );
    }
  }
  return {
    model_schema_version: 1,
    run_id: runId,
    through_sequence: through,
    through_elapsed_ns: elapsed,
    usages,
    aggregate,
    scoped_aggregates: scopedAggregates,
  };
}

function decodeMaterializedModel(
  value: unknown,
  collection: string,
  expectedRunId: CanonicalUuid,
  expectedThrough: U64String,
  expectedElapsed: U64String,
  path: string,
): JsonObject {
  const model = expectObject(value, path);
  expectExactFields(
    model,
    ["model_schema_version", "run_id", "through_sequence", "through_elapsed_ns", collection],
    path,
  );
  requireApiVersion(model.model_schema_version, `${path}.model_schema_version`);
  const runId = decodeCanonicalUuid(model.run_id, `${path}.run_id`);
  const through = decodeU64(model.through_sequence, `${path}.through_sequence`);
  const elapsed = decodeU64(model.through_elapsed_ns, `${path}.through_elapsed_ns`);
  if (runId !== expectedRunId || through !== expectedThrough || elapsed !== expectedElapsed) {
    failProtocol("snapshot_identity", path, "materialized model differs from its snapshot envelope");
  }
  expectArray(model[collection], `${path}.${collection}`).forEach((item, index) => {
    decodeJsonValue(item, `${path}.${collection}[${index}]`);
  });
  return decodeJsonValue(model, path) as JsonObject;
}

function decodeSnapshotTruncation(
  value: unknown,
  expectedThrough: U64String,
  path: string,
): SnapshotTruncation {
  const truncation = expectObject(value, path);
  const source = expectEnum(truncation.source, ["agent_message", "agent_plan"], `${path}.source`);
  expectExactFields(
    truncation,
    source === "agent_message"
      ? ["source", "sequence", "scope", "message_id"]
      : ["source", "sequence", "scope"],
    path,
  );
  const sequence = decodeU64(truncation.sequence, `${path}.sequence`);
  if (sequence === "0" || BigInt(sequence) > BigInt(expectedThrough)) {
    failProtocol("snapshot_truncation", `${path}.sequence`, "truncation is outside the captured prefix");
  }
  const scope = decodeDiagnosticScope(truncation.scope, `${path}.scope`);
  if (source === "agent_message") {
    return {
      source,
      sequence,
      scope,
      message_id: decodeRunLocalId(truncation.message_id, `${path}.message_id`),
    };
  }
  return { source, sequence, scope };
}

function decodeSnapshotGap(value: unknown, path: string): ObservationGapEvent {
  const gap = expectObject(value, path);
  expectExactFields(
    gap,
    [
      "schema_version",
      "run_id",
      "sequence",
      "elapsed_ns",
      "scope",
      "caused_by",
      "producer",
      "component",
      "reason",
      "dropped_count",
      "affected_elapsed",
      "affected_kind",
      "affected_scope",
    ],
    path,
  );
  const event = decodeDiagnosticEvent({ kind: "observation_gap", ...gap }, path);
  if (event.kind !== "observation_gap") {
    failProtocol("snapshot_gap", path, "expected an observation gap");
  }
  return event;
}

export function decodeVersionedApiObject(value: unknown, path = "response"): VersionedApiObject {
  const response = expectObject(value, path);
  if (
    !Object.prototype.hasOwnProperty.call(response, "api_schema_version")
    || !Object.prototype.hasOwnProperty.call(response, "run_id")
  ) {
    failProtocol("fields", path, "versioned API object requires api_schema_version and run_id");
  }
  requireApiVersion(response.api_schema_version, `${path}.api_schema_version`);
  decodeCanonicalUuid(response.run_id, `${path}.run_id`);
  decodeJsonValue(response, path);
  return response as unknown as VersionedApiObject;
}

export function decodeSnapshotResponse(value: unknown, path = "snapshot"): SnapshotResponse {
  const response = expectObject(value, path);
  expectExactFields(
    response,
    ["api_schema_version", "run_id", "watermark_sequence", "earliest_available_sequence", "state"],
    path,
  );
  requireApiVersion(response.api_schema_version, `${path}.api_schema_version`);
  const runId = decodeCanonicalUuid(response.run_id, `${path}.run_id`);
  const watermark = decodeU64(response.watermark_sequence, `${path}.watermark_sequence`);
  const earliest = response.earliest_available_sequence === null
    ? null
    : decodeU64(response.earliest_available_sequence, `${path}.earliest_available_sequence`);
  const state = expectObject(response.state, `${path}.state`);
  expectExactFields(
    state,
    [
      "model_schema_version",
      "run_id",
      "through_sequence",
      "through_elapsed_ns",
      "spans",
      "messages",
      "plans",
      "counters",
      "usage",
      "gaps",
      "truncations",
    ],
    `${path}.state`,
  );
  requireApiVersion(state.model_schema_version, `${path}.state.model_schema_version`);
  const stateRunId = decodeCanonicalUuid(state.run_id, `${path}.state.run_id`);
  const stateThrough = decodeU64(state.through_sequence, `${path}.state.through_sequence`);
  const stateElapsed = decodeU64(state.through_elapsed_ns, `${path}.state.through_elapsed_ns`);
  if (stateRunId !== runId || stateThrough !== watermark) {
    failProtocol("snapshot_identity", `${path}.state`, "snapshot state differs from its response envelope");
  }
  if (watermark === "0" && stateElapsed !== "0") {
    failProtocol("snapshot_identity", `${path}.state.through_elapsed_ns`, "empty snapshot must start at zero");
  }
  if ((watermark === "0") !== (earliest === null)) {
    failProtocol("snapshot", path, "empty watermark and earliest replay sequence disagree");
  }
  if (earliest !== null && (earliest === "0" || BigInt(earliest) > BigInt(watermark))) {
    failProtocol("snapshot", `${path}.earliest_available_sequence`, "replay range is invalid");
  }
  const spans = decodeMaterializedModel(
    state.spans, "spans", runId, watermark, stateElapsed, `${path}.state.spans`,
  );
  const messages = decodeMaterializedModel(
    state.messages, "messages", runId, watermark, stateElapsed, `${path}.state.messages`,
  );
  const plans = decodeMaterializedModel(
    state.plans, "plans", runId, watermark, stateElapsed, `${path}.state.plans`,
  );
  const counters = decodeMaterializedModel(
    state.counters, "series", runId, watermark, stateElapsed, `${path}.state.counters`,
  );
  const usage = decodeUsageSnapshot(
    state.usage, runId, watermark, stateElapsed, `${path}.state.usage`,
  );
  let previousGapSequence = 0n;
  const gaps = expectArray(state.gaps, `${path}.state.gaps`).map((item, index) => {
    const gap = decodeSnapshotGap(item, `${path}.state.gaps[${index}]`);
    const sequence = BigInt(gap.sequence);
    if (
      gap.kind !== "observation_gap"
      || gap.run_id !== runId
      || sequence <= previousGapSequence
      || sequence > BigInt(watermark)
      || BigInt(gap.elapsed_ns) > BigInt(stateElapsed)
    ) {
      failProtocol(
        "snapshot_gap",
        `${path}.state.gaps[${index}]`,
        "gap is outside canonical snapshot order",
      );
    }
    previousGapSequence = sequence;
    return gap;
  });
  let previousTruncationSequence = 0n;
  const truncations = expectArray(state.truncations, `${path}.state.truncations`)
    .map((item, index) => {
      const truncation = decodeSnapshotTruncation(
        item,
        watermark,
        `${path}.state.truncations[${index}]`,
      );
      const sequence = BigInt(truncation.sequence);
      if (sequence <= previousTruncationSequence) {
        failProtocol(
          "snapshot_truncation",
          `${path}.state.truncations[${index}].sequence`,
          "truncations are outside canonical snapshot order",
        );
      }
      previousTruncationSequence = sequence;
      return truncation;
    });
  return {
    api_schema_version: 1,
    run_id: runId,
    watermark_sequence: watermark,
    earliest_available_sequence: earliest,
    state: {
      model_schema_version: 1,
      run_id: stateRunId,
      through_sequence: stateThrough,
      through_elapsed_ns: stateElapsed,
      spans,
      messages,
      plans,
      counters,
      usage,
      gaps,
      truncations,
    },
  };
}

export function decodeEventsResponse(value: unknown, path = "events_response"): EventsResponse {
  const response = expectObject(value, path);
  expectExactFields(
    response,
    ["api_schema_version", "run_id", "captured_watermark", "events", "next_after"],
    path,
  );
  requireApiVersion(response.api_schema_version, `${path}.api_schema_version`);
  const runId = decodeCanonicalUuid(response.run_id, `${path}.run_id`);
  const watermark = decodeU64(response.captured_watermark, `${path}.captured_watermark`);
  const events = expectArray(response.events, `${path}.events`);
  let previous = 0n;
  events.forEach((raw, index) => {
    const event = decodeDiagnosticEvent(raw, `${path}.events[${index}]`);
    const sequence = BigInt(event.sequence);
    if (event.run_id !== runId) {
      failProtocol("run_id", `${path}.events[${index}].run_id`, "event belongs to another run");
    }
    if (sequence <= previous || sequence > BigInt(watermark)) {
      failProtocol("sequence", `${path}.events[${index}].sequence`, "event is outside captured order");
    }
    previous = sequence;
  });
  const nextAfter = response.next_after === null
    ? null
    : decodeU64(response.next_after, `${path}.next_after`);
  const finalEvent = events[events.length - 1] as { sequence: string } | undefined;
  if (nextAfter !== null && (finalEvent === undefined || nextAfter !== finalEvent.sequence)) {
    failProtocol("cursor", `${path}.next_after`, "page cursor must identify the final returned event");
  }
  return response as unknown as EventsResponse;
}

export function decodeHttpErrorResponse(value: unknown, path = "error_response"): HttpErrorResponse {
  const response = expectObject(value, path);
  expectExactFields(response, ["api_schema_version", "run_id", "error"], path);
  requireApiVersion(response.api_schema_version, `${path}.api_schema_version`);
  if (response.run_id !== null) {
    decodeCanonicalUuid(response.run_id, `${path}.run_id`);
  }
  const error = expectObject(response.error, `${path}.error`);
  expectExactFields(error, ["code", "message", "details"], `${path}.error`);
  const code = expectString(error.code, `${path}.error.code`);
  const message = expectString(error.message, `${path}.error.message`);
  if (code.length === 0 || message.length === 0) {
    failProtocol("http_error", `${path}.error`, "error code and message must be nonempty");
  }
  if (error.details !== null) {
    decodeJsonValue(expectObject(error.details, `${path}.error.details`), `${path}.error.details`);
  }
  return response as unknown as HttpErrorResponse;
}
