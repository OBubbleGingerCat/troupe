import {
  type CanonicalIntegerString,
  type CanonicalUuid,
  type DecimalString,
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
  type AgentMessageCompletedEvent,
  type AgentPlanSnapshotEvent,
  type CausalLink,
  type ContextUsageSampledEvent,
  type CounterSampledEvent,
  type CustomCounterSampledEvent,
  type CustomSpanStartedEvent,
  type DiagnosticEvent,
  type DiagnosticScope,
  type ObservationGapEvent,
  type SpanFinishedEvent,
  type SpanStartedEvent,
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
  readonly spans: SpanSnapshot;
  readonly messages: MessageSnapshot;
  readonly plans: PlanSnapshot;
  readonly counters: CounterSnapshot;
  readonly usage: UsageSnapshot;
  readonly gaps: readonly ObservationGapEvent[];
  readonly truncations: readonly SnapshotTruncation[];
}

export interface MaterializedSnapshotModel {
  readonly model_schema_version: 1;
  readonly run_id: CanonicalUuid;
  readonly through_sequence: U64String;
  readonly through_elapsed_ns: U64String;
}

export type ProjectedSpanDefinition =
  | {
    readonly family: "built_in";
    readonly detail: Pick<SpanStartedEvent, "span_kind" | "detail">;
  }
  | {
    readonly family: "custom";
    readonly name: string;
    readonly attributes: CustomSpanStartedEvent["attributes"];
  };

export interface ProjectedSpanCompletion {
  readonly finish_sequence: U64String;
  readonly finished_at_ns: U64String;
  readonly outcome: SpanFinishedEvent["outcome"];
  readonly error_code: string | null;
  readonly caused_by: readonly CausalLink[];
}

export interface ProjectedSpanSnapshot {
  readonly run_id: CanonicalUuid;
  readonly span_id: U64String;
  readonly started_at_ns: U64String;
  readonly scope: DiagnosticScope;
  readonly parent_span_id: U64String | null;
  readonly started_caused_by: readonly CausalLink[];
  readonly definition: ProjectedSpanDefinition;
  readonly completion: ProjectedSpanCompletion | null;
}

export interface SpanSnapshot extends MaterializedSnapshotModel {
  readonly spans: readonly ProjectedSpanSnapshot[];
}

export interface ProjectedMessageCompletion {
  readonly sequence: U64String;
  readonly elapsed_ns: U64String;
  readonly utf8_bytes: U64String;
  readonly unicode_scalar_count: U64String;
  readonly truncated: boolean;
  readonly caused_by: readonly CausalLink[];
}

export interface ProjectedMessageSnapshot {
  readonly run_id: CanonicalUuid;
  readonly message_id: string;
  readonly scope: DiagnosticScope;
  readonly first_sequence: U64String;
  readonly first_elapsed_ns: U64String;
  readonly latest_sequence: U64String;
  readonly latest_elapsed_ns: U64String;
  readonly source_message_id: string | null;
  readonly text: string;
  readonly completion: ProjectedMessageCompletion | null;
}

export interface MessageSnapshot extends MaterializedSnapshotModel {
  readonly messages: readonly ProjectedMessageSnapshot[];
}

export interface ProjectedPlanSnapshot {
  readonly run_id: CanonicalUuid;
  readonly scope: DiagnosticScope;
  readonly sequence: U64String;
  readonly elapsed_ns: U64String;
  readonly entries: AgentPlanSnapshotEvent["entries"];
  readonly truncated: boolean;
  readonly caused_by: readonly CausalLink[];
}

export interface PlanSnapshot extends MaterializedSnapshotModel {
  readonly plans: readonly ProjectedPlanSnapshot[];
}

export type ProjectedCounterIdentity =
  | {
    readonly family: "built_in";
    readonly scope: DiagnosticScope;
    readonly counter_kind: CounterSampledEvent["counter_kind"];
  }
  | {
    readonly family: "custom";
    readonly scope: DiagnosticScope;
    readonly name: string;
    readonly unit: string | null;
    readonly dimensions: CustomCounterSampledEvent["dimensions"];
  };

export type ProjectedCounterValue =
  | { readonly type: "unsigned"; readonly value: U64String }
  | { readonly type: "integer"; readonly value: CanonicalIntegerString }
  | { readonly type: "decimal"; readonly value: DecimalString };

export interface ProjectedCounterSnapshot {
  readonly run_id: CanonicalUuid;
  readonly series_key: string;
  readonly identity: ProjectedCounterIdentity;
  readonly sequence: U64String;
  readonly elapsed_ns: U64String;
  readonly value: ProjectedCounterValue;
  readonly caused_by: readonly CausalLink[];
}

export interface CounterSnapshot extends MaterializedSnapshotModel {
  readonly series: readonly ProjectedCounterSnapshot[];
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
  readonly contexts: readonly ContextUsageSampledEvent[];
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
  const actId = decodeRunLocalId(projected.act_id, `${path}.act_id`);
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

function decodeProjectedContextUsage(value: unknown, path: string): ContextUsageSampledEvent {
  const projected = expectObject(value, path);
  expectExactFields(
    projected,
    [
      "run_id",
      "scope",
      "sequence",
      "elapsed_ns",
      "caused_by",
      "context_used_tokens",
      "context_window_tokens",
      "cumulative_cost_amount",
      "cumulative_cost_currency",
      "sample_origin",
      "observed_elapsed_ns",
    ],
    path,
  );
  const event = decodeDiagnosticEvent({
    kind: "context_usage_sampled",
    schema_version: 1,
    ...projected,
  }, path);
  if (event.kind !== "context_usage_sampled") {
    failProtocol("usage_context", path, "expected a context usage sample");
  }
  return event;
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
      "contexts",
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
  const contexts = expectArray(usage.contexts, `${path}.contexts`).map((item, index) => (
    decodeProjectedContextUsage(item, `${path}.contexts[${index}]`)
  ));
  const contextScopes = new Set<string>();
  const materializedSequences = new Set<string>();
  let previousContextSequence = 0n;
  for (const context of contexts) {
    const identity = JSON.stringify(canonicalScope(context.scope));
    const sequence = BigInt(context.sequence);
    if (
      context.run_id !== runId
      || sequence <= previousContextSequence
      || sequence > BigInt(through)
      || BigInt(context.elapsed_ns) > BigInt(elapsed)
      || contextScopes.has(identity)
      || materializedSequences.has(context.sequence)
    ) {
      failProtocol(
        "usage_context",
        `${path}.contexts`,
        "context samples are not a unique captured projection",
      );
    }
    previousContextSequence = sequence;
    contextScopes.add(identity);
    materializedSequences.add(context.sequence);
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
      || materializedSequences.has(item.event.sequence)
    ) {
      failProtocol("usage_identity", `${path}.usages`, "projected usages are not a unique ordered prefix");
    }
    previous = sequence;
    actIds.add(item.act_id);
    materializedSequences.add(item.event.sequence);
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
    contexts,
    usages,
    aggregate,
    scoped_aggregates: scopedAggregates,
  };
}

interface MaterializedEnvelope {
  readonly run_id: CanonicalUuid;
  readonly through_sequence: U64String;
  readonly through_elapsed_ns: U64String;
  readonly items: readonly unknown[];
}

function decodeMaterializedEnvelope(
  value: unknown,
  collection: string,
  expectedRunId: CanonicalUuid,
  expectedThrough: U64String,
  expectedElapsed: U64String,
  path: string,
): MaterializedEnvelope {
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
  return {
    run_id: runId,
    through_sequence: through,
    through_elapsed_ns: elapsed,
    items: expectArray(model[collection], `${path}.${collection}`),
  };
}

function canonicalScope(scope: DiagnosticScope): DiagnosticScope {
  return {
    scene_id: scope.scene_id,
    actor_id: scope.actor_id,
    cue_id: scope.cue_id,
    effect_id: scope.effect_id,
    act_id: scope.act_id,
    tool_call_id: scope.tool_call_id,
    session_generation: scope.session_generation,
  };
}

function materializedEventPosition(
  event: DiagnosticEvent,
  expectedRunId: CanonicalUuid,
  expectedThrough: U64String,
  expectedElapsed: U64String,
  path: string,
): void {
  if (
    event.run_id !== expectedRunId
    || BigInt(event.sequence) > BigInt(expectedThrough)
    || BigInt(event.elapsed_ns) > BigInt(expectedElapsed)
  ) {
    failProtocol("snapshot_fact", path, "materialized fact is outside the captured prefix");
  }
}

function decodeProjectedSpan(
  value: unknown,
  expectedRunId: CanonicalUuid,
  expectedThrough: U64String,
  expectedElapsed: U64String,
  path: string,
): ProjectedSpanSnapshot {
  const row = expectObject(value, path);
  expectExactFields(
    row,
    [
      "run_id",
      "span_id",
      "started_at_ns",
      "scope",
      "parent_span_id",
      "started_caused_by",
      "definition",
      "completion",
    ],
    path,
  );
  const runId = decodeCanonicalUuid(row.run_id, `${path}.run_id`);
  const spanId = decodeU64(row.span_id, `${path}.span_id`);
  const startedAt = decodeU64(row.started_at_ns, `${path}.started_at_ns`);
  const scope = canonicalScope(decodeDiagnosticScope(row.scope, `${path}.scope`));
  const definition = expectObject(row.definition, `${path}.definition`);
  const family = expectEnum(
    definition.family,
    ["built_in", "custom"],
    `${path}.definition.family`,
  );
  const common = {
    schema_version: 1,
    run_id: runId,
    sequence: spanId,
    elapsed_ns: startedAt,
    scope,
    caused_by: row.started_caused_by,
    parent_span_id: row.parent_span_id,
  } as const;
  let start: SpanStartedEvent | CustomSpanStartedEvent;
  let decodedDefinition: ProjectedSpanDefinition;
  if (family === "built_in") {
    expectExactFields(definition, ["family", "detail"], `${path}.definition`);
    const detail = expectObject(definition.detail, `${path}.definition.detail`);
    expectExactFields(detail, ["span_kind", "detail"], `${path}.definition.detail`);
    const event = decodeDiagnosticEvent({
      ...common,
      kind: "span_started",
      span_kind: detail.span_kind,
      detail: detail.detail,
    }, path);
    if (event.kind !== "span_started") {
      failProtocol("snapshot_span", path, "expected a built-in span start");
    }
    start = event;
    decodedDefinition = {
      family,
      detail: { span_kind: event.span_kind, detail: event.detail },
    };
  } else {
    expectExactFields(definition, ["family", "name", "attributes"], `${path}.definition`);
    const event = decodeDiagnosticEvent({
      ...common,
      kind: "custom_span_started",
      name: definition.name,
      attributes: definition.attributes,
    }, path);
    if (event.kind !== "custom_span_started") {
      failProtocol("snapshot_span", path, "expected a custom span start");
    }
    start = event;
    decodedDefinition = {
      family,
      name: event.name,
      attributes: event.attributes,
    };
  }
  materializedEventPosition(start, expectedRunId, expectedThrough, expectedElapsed, path);
  if (
    start.parent_span_id !== null
    && (start.parent_span_id === "0" || BigInt(start.parent_span_id) >= BigInt(spanId))
  ) {
    failProtocol("snapshot_span", `${path}.parent_span_id`, "parent span is not earlier");
  }

  let completion: ProjectedSpanCompletion | null = null;
  if (row.completion !== null) {
    const rawCompletion = expectObject(row.completion, `${path}.completion`);
    expectExactFields(
      rawCompletion,
      ["finish_sequence", "finished_at_ns", "outcome", "error_code", "caused_by"],
      `${path}.completion`,
    );
    const finishCommon = {
      schema_version: 1,
      run_id: runId,
      sequence: rawCompletion.finish_sequence,
      elapsed_ns: rawCompletion.finished_at_ns,
      scope,
      caused_by: rawCompletion.caused_by,
      span_id: spanId,
      outcome: rawCompletion.outcome,
    } as const;
    const finish = decodeDiagnosticEvent(family === "built_in"
      ? { ...finishCommon, kind: "span_finished", error_code: rawCompletion.error_code }
      : { ...finishCommon, kind: "custom_span_finished" }, `${path}.completion`);
    if (
      finish.kind !== "span_finished"
      && finish.kind !== "custom_span_finished"
    ) {
      failProtocol("snapshot_span", `${path}.completion`, "expected a span completion");
    }
    if (
      (family === "built_in" && finish.kind !== "span_finished")
      || (family === "custom" && (
        finish.kind !== "custom_span_finished" || rawCompletion.error_code !== null
      ))
    ) {
      failProtocol("snapshot_span", `${path}.completion`, "completion family does not match span");
    }
    materializedEventPosition(finish, expectedRunId, expectedThrough, expectedElapsed, path);
    if (
      BigInt(finish.sequence) <= BigInt(spanId)
      || BigInt(finish.elapsed_ns) < BigInt(startedAt)
    ) {
      failProtocol("snapshot_span", `${path}.completion`, "completion is not after its start");
    }
    completion = {
      finish_sequence: finish.sequence,
      finished_at_ns: finish.elapsed_ns,
      outcome: finish.outcome,
      error_code: finish.kind === "span_finished" ? finish.error_code : null,
      caused_by: finish.caused_by,
    };
  }
  return {
    run_id: start.run_id,
    span_id: start.sequence,
    started_at_ns: start.elapsed_ns,
    scope: start.scope,
    parent_span_id: start.parent_span_id,
    started_caused_by: start.caused_by,
    definition: decodedDefinition,
    completion,
  };
}

function decodeSpanSnapshot(
  value: unknown,
  expectedRunId: CanonicalUuid,
  expectedThrough: U64String,
  expectedElapsed: U64String,
  path: string,
): SpanSnapshot {
  const envelope = decodeMaterializedEnvelope(
    value, "spans", expectedRunId, expectedThrough, expectedElapsed, path,
  );
  const spans = envelope.items.map((item, index) => decodeProjectedSpan(
    item,
    expectedRunId,
    expectedThrough,
    expectedElapsed,
    `${path}.spans[${index}]`,
  ));
  const spanIds = new Set<string>();
  const sequences = new Set<string>();
  for (const span of spans) {
    if (spanIds.has(span.span_id) || sequences.has(span.span_id)) {
      failProtocol("snapshot_span", `${path}.spans`, "span identities are not unique");
    }
    spanIds.add(span.span_id);
    sequences.add(span.span_id);
    if (span.completion !== null) {
      if (sequences.has(span.completion.finish_sequence)) {
        failProtocol("snapshot_span", `${path}.spans`, "span event sequences are not unique");
      }
      sequences.add(span.completion.finish_sequence);
    }
  }
  for (const span of spans) {
    if (span.parent_span_id !== null && !spanIds.has(span.parent_span_id)) {
      failProtocol("snapshot_span", `${path}.spans`, "parent span is absent from the read model");
    }
  }
  return {
    model_schema_version: 1,
    run_id: envelope.run_id,
    through_sequence: envelope.through_sequence,
    through_elapsed_ns: envelope.through_elapsed_ns,
    spans,
  };
}

function decodeProjectedMessage(
  value: unknown,
  expectedRunId: CanonicalUuid,
  expectedThrough: U64String,
  expectedElapsed: U64String,
  path: string,
): ProjectedMessageSnapshot {
  const row = expectObject(value, path);
  expectExactFields(
    row,
    [
      "run_id",
      "message_id",
      "scope",
      "first_sequence",
      "first_elapsed_ns",
      "latest_sequence",
      "latest_elapsed_ns",
      "source_message_id",
      "text",
      "completion",
    ],
    path,
  );
  const runId = decodeCanonicalUuid(row.run_id, `${path}.run_id`);
  const messageId = decodeRunLocalId(row.message_id, `${path}.message_id`);
  const scope = canonicalScope(decodeDiagnosticScope(row.scope, `${path}.scope`));
  const firstSequence = decodeU64(row.first_sequence, `${path}.first_sequence`);
  const firstElapsed = decodeU64(row.first_elapsed_ns, `${path}.first_elapsed_ns`);
  const latestSequence = decodeU64(row.latest_sequence, `${path}.latest_sequence`);
  const latestElapsed = decodeU64(row.latest_elapsed_ns, `${path}.latest_elapsed_ns`);
  if (
    runId !== expectedRunId
    || firstSequence === "0"
    || BigInt(firstSequence) > BigInt(latestSequence)
    || BigInt(latestSequence) > BigInt(expectedThrough)
    || BigInt(firstElapsed) > BigInt(latestElapsed)
    || BigInt(latestElapsed) > BigInt(expectedElapsed)
  ) {
    failProtocol("snapshot_message", path, "message is outside the captured prefix");
  }
  const sourceMessageId = row.source_message_id === null
    ? null
    : expectString(row.source_message_id, `${path}.source_message_id`);
  const text = expectString(row.text, `${path}.text`);
  let completion: ProjectedMessageCompletion | null = null;
  if (row.completion !== null) {
    const rawCompletion = expectObject(row.completion, `${path}.completion`);
    expectExactFields(
      rawCompletion,
      ["sequence", "elapsed_ns", "utf8_bytes", "unicode_scalar_count", "truncated", "caused_by"],
      `${path}.completion`,
    );
    const event = decodeDiagnosticEvent({
      kind: "agent_message_completed",
      schema_version: 1,
      run_id: runId,
      sequence: rawCompletion.sequence,
      elapsed_ns: rawCompletion.elapsed_ns,
      scope,
      caused_by: rawCompletion.caused_by,
      message_id: messageId,
      utf8_bytes: rawCompletion.utf8_bytes,
      unicode_scalar_count: rawCompletion.unicode_scalar_count,
      truncated: rawCompletion.truncated,
    }, `${path}.completion`);
    if (
      event.kind !== "agent_message_completed"
      || event.sequence !== latestSequence
      || event.elapsed_ns !== latestElapsed
    ) {
      failProtocol("snapshot_message", `${path}.completion`, "completion is not the latest message fact");
    }
    materializedEventPosition(event, expectedRunId, expectedThrough, expectedElapsed, path);
    completion = {
      sequence: event.sequence,
      elapsed_ns: event.elapsed_ns,
      utf8_bytes: event.utf8_bytes,
      unicode_scalar_count: event.unicode_scalar_count,
      truncated: event.truncated,
      caused_by: event.caused_by,
    };
  }
  return {
    run_id: runId,
    message_id: messageId,
    scope,
    first_sequence: firstSequence,
    first_elapsed_ns: firstElapsed,
    latest_sequence: latestSequence,
    latest_elapsed_ns: latestElapsed,
    source_message_id: sourceMessageId,
    text,
    completion,
  };
}

function decodeMessageSnapshot(
  value: unknown,
  expectedRunId: CanonicalUuid,
  expectedThrough: U64String,
  expectedElapsed: U64String,
  path: string,
): MessageSnapshot {
  const envelope = decodeMaterializedEnvelope(
    value, "messages", expectedRunId, expectedThrough, expectedElapsed, path,
  );
  const messages = envelope.items.map((item, index) => decodeProjectedMessage(
    item,
    expectedRunId,
    expectedThrough,
    expectedElapsed,
    `${path}.messages[${index}]`,
  ));
  const messageIds = new Set<string>();
  const endpointSequences = new Set<string>();
  for (const message of messages) {
    if (
      messageIds.has(message.message_id)
      || endpointSequences.has(message.first_sequence)
      || (message.latest_sequence !== message.first_sequence
        && endpointSequences.has(message.latest_sequence))
    ) {
      failProtocol(
        "snapshot_message",
        `${path}.messages`,
        "message identities and endpoint sequences are not unique",
      );
    }
    messageIds.add(message.message_id);
    endpointSequences.add(message.first_sequence);
    endpointSequences.add(message.latest_sequence);
  }
  return {
    model_schema_version: 1,
    run_id: envelope.run_id,
    through_sequence: envelope.through_sequence,
    through_elapsed_ns: envelope.through_elapsed_ns,
    messages,
  };
}

function decodeProjectedPlan(
  value: unknown,
  expectedRunId: CanonicalUuid,
  expectedThrough: U64String,
  expectedElapsed: U64String,
  path: string,
): ProjectedPlanSnapshot {
  const row = expectObject(value, path);
  expectExactFields(
    row,
    ["run_id", "scope", "sequence", "elapsed_ns", "entries", "truncated", "caused_by"],
    path,
  );
  const event = decodeDiagnosticEvent({
    kind: "agent_plan_snapshot",
    schema_version: 1,
    run_id: row.run_id,
    sequence: row.sequence,
    elapsed_ns: row.elapsed_ns,
    scope: canonicalScope(decodeDiagnosticScope(row.scope, `${path}.scope`)),
    caused_by: row.caused_by,
    entries: row.entries,
    truncated: row.truncated,
  }, path);
  if (event.kind !== "agent_plan_snapshot") {
    failProtocol("snapshot_plan", path, "expected a plan snapshot");
  }
  materializedEventPosition(event, expectedRunId, expectedThrough, expectedElapsed, path);
  return {
    run_id: event.run_id,
    scope: event.scope,
    sequence: event.sequence,
    elapsed_ns: event.elapsed_ns,
    entries: event.entries,
    truncated: event.truncated,
    caused_by: event.caused_by,
  };
}

function scopeKey(scope: DiagnosticScope): string {
  return JSON.stringify(canonicalScope(scope));
}

function decodePlanSnapshot(
  value: unknown,
  expectedRunId: CanonicalUuid,
  expectedThrough: U64String,
  expectedElapsed: U64String,
  path: string,
): PlanSnapshot {
  const envelope = decodeMaterializedEnvelope(
    value, "plans", expectedRunId, expectedThrough, expectedElapsed, path,
  );
  const plans = envelope.items.map((item, index) => decodeProjectedPlan(
    item,
    expectedRunId,
    expectedThrough,
    expectedElapsed,
    `${path}.plans[${index}]`,
  ));
  const scopes = new Set<string>();
  const actScopes = new Map<string, string>();
  const sequences = new Set<string>();
  for (const plan of plans) {
    const key = scopeKey(plan.scope);
    if (scopes.has(key) || sequences.has(plan.sequence)) {
      failProtocol("snapshot_plan", `${path}.plans`, "plan scopes and sequences are not unique");
    }
    scopes.add(key);
    sequences.add(plan.sequence);
    const actId = plan.scope.act_id;
    if (actId !== null) {
      const previous = actScopes.get(actId);
      if (previous !== undefined && previous !== key) {
        failProtocol("snapshot_plan", `${path}.plans`, "one Act has multiple plan scopes");
      }
      actScopes.set(actId, key);
    }
  }
  return {
    model_schema_version: 1,
    run_id: envelope.run_id,
    through_sequence: envelope.through_sequence,
    through_elapsed_ns: envelope.through_elapsed_ns,
    plans,
  };
}

function canonicalDimensions(
  dimensions: CustomCounterSampledEvent["dimensions"],
): CustomCounterSampledEvent["dimensions"] {
  const ordered: Record<string, CustomCounterSampledEvent["dimensions"][string]> = {};
  for (const key of Object.keys(dimensions).sort()) {
    const scalar = dimensions[key]!;
    ordered[key] = { type: scalar.type, value: scalar.value } as typeof scalar;
  }
  return ordered;
}

function decodeProjectedCounter(
  value: unknown,
  expectedRunId: CanonicalUuid,
  expectedThrough: U64String,
  expectedElapsed: U64String,
  path: string,
): ProjectedCounterSnapshot {
  const row = expectObject(value, path);
  expectExactFields(
    row,
    ["run_id", "series_key", "identity", "sequence", "elapsed_ns", "value", "caused_by"],
    path,
  );
  const identity = expectObject(row.identity, `${path}.identity`);
  const family = expectEnum(identity.family, ["built_in", "custom"], `${path}.identity.family`);
  const projectedValue = expectObject(row.value, `${path}.value`);
  expectExactFields(projectedValue, ["type", "value"], `${path}.value`);
  const valueType = expectEnum(
    projectedValue.type,
    ["unsigned", "integer", "decimal"],
    `${path}.value.type`,
  );
  const common = {
    schema_version: 1,
    run_id: row.run_id,
    sequence: row.sequence,
    elapsed_ns: row.elapsed_ns,
    caused_by: row.caused_by,
  } as const;
  let event: CounterSampledEvent | CustomCounterSampledEvent;
  let decodedIdentity: ProjectedCounterIdentity;
  let decodedValue: ProjectedCounterValue;
  if (family === "built_in") {
    expectExactFields(identity, ["family", "scope", "counter_kind"], `${path}.identity`);
    if (valueType !== "unsigned") {
      failProtocol("snapshot_counter", `${path}.value`, "built-in counter requires unsigned value");
    }
    const candidate = decodeDiagnosticEvent({
      ...common,
      kind: "counter_sampled",
      scope: canonicalScope(decodeDiagnosticScope(identity.scope, `${path}.identity.scope`)),
      counter_kind: identity.counter_kind,
      value: projectedValue.value,
    }, path);
    if (candidate.kind !== "counter_sampled") {
      failProtocol("snapshot_counter", path, "expected a built-in counter sample");
    }
    event = candidate;
    decodedIdentity = {
      family,
      scope: event.scope,
      counter_kind: event.counter_kind,
    };
    decodedValue = { type: "unsigned", value: event.value };
  } else {
    expectExactFields(
      identity,
      ["family", "scope", "name", "unit", "dimensions"],
      `${path}.identity`,
    );
    if (valueType === "unsigned") {
      failProtocol("snapshot_counter", `${path}.value`, "custom counter requires integer or decimal value");
    }
    const candidate = decodeDiagnosticEvent({
      ...common,
      kind: "custom_counter_sampled",
      scope: canonicalScope(decodeDiagnosticScope(identity.scope, `${path}.identity.scope`)),
      name: identity.name,
      unit: identity.unit,
      dimensions: identity.dimensions,
      value: { type: valueType, value: projectedValue.value },
    }, path);
    if (candidate.kind !== "custom_counter_sampled") {
      failProtocol("snapshot_counter", path, "expected a custom counter sample");
    }
    event = candidate;
    const dimensions = canonicalDimensions(event.dimensions);
    decodedIdentity = {
      family,
      scope: event.scope,
      name: event.name,
      unit: event.unit,
      dimensions,
    };
    decodedValue = event.value;
  }
  materializedEventPosition(event, expectedRunId, expectedThrough, expectedElapsed, path);
  const seriesKey = expectString(row.series_key, `${path}.series_key`);
  if (seriesKey !== JSON.stringify(decodedIdentity)) {
    failProtocol("snapshot_counter", `${path}.series_key`, "series key is not canonical for identity");
  }
  return {
    run_id: event.run_id,
    series_key: seriesKey,
    identity: decodedIdentity,
    sequence: event.sequence,
    elapsed_ns: event.elapsed_ns,
    value: decodedValue,
    caused_by: event.caused_by,
  };
}

function decodeCounterSnapshot(
  value: unknown,
  expectedRunId: CanonicalUuid,
  expectedThrough: U64String,
  expectedElapsed: U64String,
  path: string,
): CounterSnapshot {
  const envelope = decodeMaterializedEnvelope(
    value, "series", expectedRunId, expectedThrough, expectedElapsed, path,
  );
  const series = envelope.items.map((item, index) => decodeProjectedCounter(
    item,
    expectedRunId,
    expectedThrough,
    expectedElapsed,
    `${path}.series[${index}]`,
  ));
  const keys = new Set<string>();
  const sequences = new Set<string>();
  for (const sample of series) {
    if (keys.has(sample.series_key) || sequences.has(sample.sequence)) {
      failProtocol("snapshot_counter", `${path}.series`, "counter identities and sequences must be unique");
    }
    keys.add(sample.series_key);
    sequences.add(sample.sequence);
  }
  return {
    model_schema_version: 1,
    run_id: envelope.run_id,
    through_sequence: envelope.through_sequence,
    through_elapsed_ns: envelope.through_elapsed_ns,
    series,
  };
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
  const scope = canonicalScope(decodeDiagnosticScope(truncation.scope, `${path}.scope`));
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
  const spans = decodeSpanSnapshot(
    state.spans, runId, watermark, stateElapsed, `${path}.state.spans`,
  );
  const messages = decodeMessageSnapshot(
    state.messages, runId, watermark, stateElapsed, `${path}.state.messages`,
  );
  const plans = decodePlanSnapshot(
    state.plans, runId, watermark, stateElapsed, `${path}.state.plans`,
  );
  const counters = decodeCounterSnapshot(
    state.counters, runId, watermark, stateElapsed, `${path}.state.counters`,
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
  const expectedTruncations: SnapshotTruncation[] = [
    ...messages.messages.flatMap((message): SnapshotTruncation[] => (
      message.completion?.truncated === true
        ? [{
          source: "agent_message",
          sequence: message.completion.sequence,
          scope: message.scope,
          message_id: message.message_id,
        }]
        : []
    )),
    ...plans.plans.flatMap((plan): SnapshotTruncation[] => (
      plan.truncated
        ? [{ source: "agent_plan", sequence: plan.sequence, scope: plan.scope }]
        : []
    )),
  ].sort((left, right) => {
    const leftSequence = BigInt(left.sequence);
    const rightSequence = BigInt(right.sequence);
    return leftSequence < rightSequence ? -1 : leftSequence > rightSequence ? 1 : 0;
  });
  if (
    truncations.length !== expectedTruncations.length
    || truncations.some((truncation, index) => (
      JSON.stringify(truncation) !== JSON.stringify(expectedTruncations[index])
    ))
  ) {
    failProtocol(
      "snapshot_truncation",
      `${path}.state.truncations`,
      "truncations do not match materialized message and plan facts",
    );
  }
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
