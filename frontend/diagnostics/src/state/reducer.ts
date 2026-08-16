import { compareU64 } from "../protocol/decimal.ts";
import type { CanonicalUuid, U64String } from "../protocol/decimal.ts";
import type {
  AgentMessageCompletedEvent,
  AgentMessageDeltaEvent,
  DiagnosticEvent,
  DiagnosticScope,
} from "../protocol/event.ts";
import type {
  CachedQueryResult,
  DiagnosticState,
  EventWindow,
  GapProjection,
  LiveEdgeState,
  LiveProjection,
  PauseState,
  PresentationFilters,
  PresentationState,
  ProjectedActUsage,
  ProjectedContextUsage,
  ProjectedCounter,
  ProjectedMessage,
  ProjectedResultFact,
  ProjectedSpan,
  ProjectedToolFact,
  ProjectionBucket,
  SelectionReference,
} from "./model.ts";
import {
  ACT_USAGE_CAPACITY,
  CONTEXT_USAGE_CAPACITY,
  COUNTER_SERIES_CAPACITY,
  GAP_CAPACITY,
  MESSAGE_CAPACITY,
  MESSAGE_TEXT_CODE_UNIT_CAPACITY,
  RESULT_FACT_CAPACITY,
  SPAN_CAPACITY,
  TOOL_FACT_CAPACITY,
} from "./model.ts";
import {
  cacheQueryResult,
  createQueryCache,
  invalidateAllQueries,
  invalidateQueries,
  queryDependsOnEvent,
} from "./queries.ts";
import {
  createPresentationState,
  pinDetail,
  select,
  setFilters,
  setFollowLive,
  setViewport,
  setZoom,
  toggleExpanded,
} from "./selection.ts";
import { activateWindow, appendLiveEvent, createWindowState } from "./windows.ts";


export type DiagnosticStateAction =
  | { readonly type: "event_received"; readonly event: DiagnosticEvent }
  | { readonly type: "watermark_observed"; readonly through_sequence: U64String }
  | { readonly type: "pause" }
  | { readonly type: "resume" }
  | { readonly type: "resume_request_consumed" }
  | { readonly type: "window_activated"; readonly window: EventWindow }
  | { readonly type: "query_cached"; readonly result: CachedQueryResult }
  | { readonly type: "select"; readonly selection: SelectionReference | null }
  | { readonly type: "pin_detail"; readonly selection: SelectionReference | null }
  | { readonly type: "toggle_expanded"; readonly id: string }
  | { readonly type: "filters_set"; readonly filters: PresentationFilters }
  | { readonly type: "viewport_set"; readonly viewport: PresentationState["viewport"] }
  | { readonly type: "follow_live_set"; readonly follow_live: boolean }
  | { readonly type: "zoom_set"; readonly zoom: PresentationState["zoom"] };

function emptyBucket<T>(baseThrough: U64String): ProjectionBucket<T> {
  return {
    base_through: baseThrough,
    items: [],
    dropped_through: null,
    needs_server_refresh: false,
  };
}

function createLiveProjection(baseThrough: U64String): LiveProjection {
  return {
    spans: emptyBucket(baseThrough),
    messages: emptyBucket(baseThrough),
    counters: emptyBucket(baseThrough),
    context_usage: emptyBucket(baseThrough),
    act_usage: emptyBucket(baseThrough),
    tools: emptyBucket(baseThrough),
    results: emptyBucket(baseThrough),
    gaps: {
      ...emptyBucket(baseThrough),
      declared_dropped_count: 0n,
      has_unknown_dropped_count: false,
    },
  };
}

function createLiveEdge(baseThrough: U64String, observedElapsedNs: U64String): LiveEdgeState {
  return {
    base_through: baseThrough,
    observed_elapsed_ns: observedElapsedNs,
    events: [],
    dropped_through: null,
    projection: createLiveProjection(baseThrough),
  };
}

export function createDiagnosticState(
  runId: CanonicalUuid,
  throughSequence: U64String,
  throughElapsedNs: U64String = "0" as U64String,
): DiagnosticState {
  return {
    run_id: runId,
    cursor: {
      delivered_through: throughSequence,
      committed_watermark: throughSequence,
    },
    delivery_issue: null,
    windows: createWindowState(),
    live: createLiveEdge(throughSequence, throughElapsedNs),
    queries: createQueryCache(),
    presentation: createPresentationState(),
    pause: {
      paused: false,
      paused_at: null,
      unseen_count: 0n,
      resume_request: null,
      frozen_live: null,
    },
  };
}

function later(left: U64String | null, right: U64String): U64String {
  return left === null || compareU64(left, right) < 0 ? right : left;
}

function upsertBounded<T>(
  bucket: ProjectionBucket<T>,
  item: T,
  capacity: number,
  keyOf: (candidate: T) => string,
  sequenceOf: (candidate: T) => U64String,
  touch = false,
): ProjectionBucket<T> {
  const key = keyOf(item);
  const existing = bucket.items.findIndex((candidate) => keyOf(candidate) === key);
  const items = [...bucket.items];
  if (existing >= 0) {
    if (touch) {
      items.splice(existing, 1);
      items.push(item);
    } else {
      items[existing] = item;
    }
  } else {
    items.push(item);
  }
  let droppedThrough = bucket.dropped_through;
  let needsServerRefresh = bucket.needs_server_refresh;
  while (items.length > capacity) {
    const dropped = items.shift();
    if (dropped !== undefined) {
      droppedThrough = later(droppedThrough, sequenceOf(dropped));
      needsServerRefresh = true;
    }
  }
  return {
    ...bucket,
    items,
    dropped_through: droppedThrough,
    needs_server_refresh: needsServerRefresh,
  };
}

function markRefresh<T>(bucket: ProjectionBucket<T>): ProjectionBucket<T> {
  return bucket.needs_server_refresh ? bucket : { ...bucket, needs_server_refresh: true };
}

function scopeKey(scope: DiagnosticScope): string {
  return JSON.stringify([
    scope.scene_id,
    scope.actor_id,
    scope.cue_id,
    scope.effect_id,
    scope.act_id,
    scope.tool_call_id,
    scope.session_generation,
  ]);
}

function stableValue(value: unknown): unknown {
  if (Array.isArray(value)) {
    return value.map((item) => stableValue(item));
  }
  if (typeof value === "object" && value !== null) {
    const record = value as Readonly<Record<string, unknown>>;
    return Object.keys(record).sort().map((key) => [key, stableValue(record[key])]);
  }
  return value;
}

function stableRecordKey(value: Readonly<Record<string, unknown>>): string {
  return JSON.stringify(stableValue(value));
}

function counterKey(event: Extract<DiagnosticEvent, { kind: "counter_sampled" | "custom_counter_sampled" }>): string {
  if (event.kind === "counter_sampled") {
    return JSON.stringify(["built_in", scopeKey(event.scope), event.counter_kind]);
  }
  return JSON.stringify([
    "custom",
    scopeKey(event.scope),
    event.name,
    event.unit,
    stableRecordKey(event.dimensions),
  ]);
}

function spanSequence(span: ProjectedSpan): U64String {
  return span.finish?.sequence ?? span.start?.sequence ?? span.span_id;
}

function projectSpan(
  bucket: ProjectionBucket<ProjectedSpan>,
  event: DiagnosticEvent,
): ProjectionBucket<ProjectedSpan> {
  if (event.kind === "span_started" || event.kind === "custom_span_started") {
    const existing = bucket.items.find((span) => span.span_id === event.sequence);
    const next: ProjectedSpan = {
      span_id: event.sequence,
      start: event,
      finish: existing?.finish ?? null,
    };
    const updated = upsertBounded(bucket, next, SPAN_CAPACITY, (span) => span.span_id, spanSequence);
    return existing?.start === undefined || existing.start === null ? updated : markRefresh(updated);
  }
  if (event.kind !== "span_finished" && event.kind !== "custom_span_finished") {
    return bucket;
  }
  const existing = bucket.items.find((span) => span.span_id === event.span_id);
  const familyMismatch = existing?.start !== null
    && existing?.start !== undefined
    && ((event.kind === "span_finished") !== (existing.start.kind === "span_started"));
  const next: ProjectedSpan = {
    span_id: event.span_id,
    start: existing?.start ?? null,
    finish: event,
  };
  const updated = upsertBounded(bucket, next, SPAN_CAPACITY, (span) => span.span_id, spanSequence);
  return existing === undefined || existing.finish !== null || familyMismatch
    ? markRefresh(updated)
    : updated;
}

function sameScope(left: DiagnosticScope, right: DiagnosticScope): boolean {
  return scopeKey(left) === scopeKey(right);
}

function trimMessageText(text: string): { readonly text: string; readonly truncated: boolean } {
  if (text.length <= MESSAGE_TEXT_CODE_UNIT_CAPACITY) {
    return { text, truncated: false };
  }
  let start = text.length - MESSAGE_TEXT_CODE_UNIT_CAPACITY;
  const codeUnit = text.charCodeAt(start);
  if (codeUnit >= 0xdc00 && codeUnit <= 0xdfff) {
    start += 1;
  }
  return { text: text.slice(start), truncated: true };
}

function messageSequence(message: ProjectedMessage): U64String {
  return message.latest_sequence;
}

function invalidMessageIdentity(
  existing: ProjectedMessage,
  event: AgentMessageDeltaEvent | AgentMessageCompletedEvent,
): boolean {
  if (!sameScope(existing.scope, event.scope)) {
    return true;
  }
  return event.kind === "agent_message_delta"
    && event.source_message_id !== null
    && existing.source_message_id !== null
    && event.source_message_id !== existing.source_message_id;
}

function projectMessage(
  bucket: ProjectionBucket<ProjectedMessage>,
  event: DiagnosticEvent,
): ProjectionBucket<ProjectedMessage> {
  if (event.kind !== "agent_message_delta" && event.kind !== "agent_message_completed") {
    return bucket;
  }
  const existing = bucket.items.find((message) => message.message_id === event.message_id);
  if (existing !== undefined && invalidMessageIdentity(existing, event)) {
    return markRefresh(bucket);
  }
  if (event.kind === "agent_message_delta") {
    if (existing?.completion !== null && existing?.completion !== undefined) {
      return markRefresh(bucket);
    }
    const trimmed = trimMessageText(`${existing?.text ?? ""}${event.text_delta}`);
    const next: ProjectedMessage = {
      message_id: event.message_id,
      scope: event.scope,
      first_sequence: existing?.first_sequence ?? event.sequence,
      latest_sequence: event.sequence,
      latest_elapsed_ns: event.elapsed_ns,
      source_message_id: existing?.source_message_id ?? event.source_message_id,
      text: trimmed.text,
      text_complete_from_start: (existing?.text_complete_from_start ?? bucket.base_through === "0")
        && !trimmed.truncated,
      text_truncated_before: (existing?.text_truncated_before ?? false) || trimmed.truncated,
      completion: null,
    };
    const updated = upsertBounded(
      bucket,
      next,
      MESSAGE_CAPACITY,
      (message) => message.message_id,
      messageSequence,
      true,
    );
    return trimmed.truncated || !next.text_complete_from_start ? markRefresh(updated) : updated;
  }
  const next: ProjectedMessage = {
    message_id: event.message_id,
    scope: event.scope,
    first_sequence: existing?.first_sequence ?? event.sequence,
    latest_sequence: event.sequence,
    latest_elapsed_ns: event.elapsed_ns,
    source_message_id: existing?.source_message_id ?? null,
    text: existing?.text ?? "",
    text_complete_from_start: existing?.text_complete_from_start ?? bucket.base_through === "0",
    text_truncated_before: existing?.text_truncated_before ?? false,
    completion: event,
  };
  const updated = upsertBounded(
    bucket,
    next,
    MESSAGE_CAPACITY,
    (message) => message.message_id,
    messageSequence,
    true,
  );
  return existing?.completion !== null && existing?.completion !== undefined
    || !next.text_complete_from_start
    ? markRefresh(updated)
    : updated;
}

function projectCounter(
  bucket: ProjectionBucket<ProjectedCounter>,
  event: DiagnosticEvent,
): ProjectionBucket<ProjectedCounter> {
  if (event.kind !== "counter_sampled" && event.kind !== "custom_counter_sampled") {
    return bucket;
  }
  const seriesKey = counterKey(event);
  const existing = bucket.items.find((counter) => counter.series_key === seriesKey);
  const item = { series_key: seriesKey, event };
  const updated = upsertBounded(
    bucket,
    item,
    COUNTER_SERIES_CAPACITY,
    (counter) => counter.series_key,
    (counter) => counter.event.sequence,
    true,
  );
  const tagMismatch = event.kind === "custom_counter_sampled"
    && existing?.event.kind === "custom_counter_sampled"
    && event.value.type !== existing.event.value.type;
  return tagMismatch ? markRefresh(updated) : updated;
}

function projectContextUsage(
  bucket: ProjectionBucket<ProjectedContextUsage>,
  event: DiagnosticEvent,
): ProjectionBucket<ProjectedContextUsage> {
  if (event.kind !== "context_usage_sampled") {
    return bucket;
  }
  const item = { scope_key: scopeKey(event.scope), event };
  return upsertBounded(
    bucket,
    item,
    CONTEXT_USAGE_CAPACITY,
    (usage) => usage.scope_key,
    (usage) => usage.event.sequence,
    true,
  );
}

function projectActUsage(
  bucket: ProjectionBucket<ProjectedActUsage>,
  event: DiagnosticEvent,
): ProjectionBucket<ProjectedActUsage> {
  if (event.kind !== "act_token_usage_finalized") {
    return bucket;
  }
  const actKey = event.scope.act_id ?? `missing-act:${event.sequence}`;
  const duplicate = bucket.items.some((usage) => usage.act_key === actKey);
  const updated = upsertBounded(
    bucket,
    { act_key: actKey, event },
    ACT_USAGE_CAPACITY,
    (usage) => usage.act_key,
    (usage) => usage.event.sequence,
    true,
  );
  return event.scope.act_id === null || duplicate ? markRefresh(updated) : updated;
}

interface ToolDetail {
  readonly title: string;
  readonly tool_kind: NonNullable<ProjectedToolFact["tool_kind"]>;
  readonly status: NonNullable<ProjectedToolFact["status"]>;
  readonly error_code: string | null;
}

function toolDetail(event: Extract<DiagnosticEvent, { kind: "span_started" | "instant_occurred" }>): ToolDetail {
  return event.detail as unknown as ToolDetail;
}

function latestToolFact(
  bucket: ProjectionBucket<ProjectedToolFact>,
  toolCallId: string | null,
  spanId: U64String | null,
): ProjectedToolFact | undefined {
  for (let index = bucket.items.length - 1; index >= 0; index -= 1) {
    const fact = bucket.items[index];
    if (
      fact !== undefined
      && ((spanId !== null && fact.span_id === spanId)
        || (toolCallId !== null && fact.tool_call_id === toolCallId))
    ) {
      return fact;
    }
  }
  return undefined;
}

function appendToolFact(
  bucket: ProjectionBucket<ProjectedToolFact>,
  fact: ProjectedToolFact,
): ProjectionBucket<ProjectedToolFact> {
  const updated = upsertBounded(
    bucket,
    fact,
    TOOL_FACT_CAPACITY,
    (candidate) => candidate.sequence,
    (candidate) => candidate.sequence,
  );
  return fact.tool_call_id === null ? markRefresh(updated) : updated;
}

function projectTool(
  bucket: ProjectionBucket<ProjectedToolFact>,
  spans: ProjectionBucket<ProjectedSpan>,
  event: DiagnosticEvent,
): ProjectionBucket<ProjectedToolFact> {
  if (event.kind === "span_started" && event.span_kind === "tool.call") {
    const detail = toolDetail(event);
    return appendToolFact(bucket, {
      phase: "started",
      sequence: event.sequence,
      elapsed_ns: event.elapsed_ns,
      scope: event.scope,
      tool_call_id: event.scope.tool_call_id,
      span_id: event.sequence,
      title: detail.title,
      tool_kind: detail.tool_kind,
      status: detail.status,
      outcome: null,
      error_code: detail.error_code,
    });
  }
  if (event.kind === "instant_occurred" && event.instant_kind === "tool.updated") {
    const previous = latestToolFact(bucket, event.scope.tool_call_id, event.containing_span_id);
    const detail = toolDetail(event);
    return appendToolFact(bucket, {
      phase: "updated",
      sequence: event.sequence,
      elapsed_ns: event.elapsed_ns,
      scope: event.scope,
      tool_call_id: event.scope.tool_call_id ?? previous?.tool_call_id ?? null,
      span_id: event.containing_span_id ?? previous?.span_id ?? null,
      title: detail.title,
      tool_kind: detail.tool_kind,
      status: detail.status,
      outcome: null,
      error_code: detail.error_code,
    });
  }
  if (event.kind !== "span_finished") {
    return bucket;
  }
  const span = spans.items.find((candidate) => candidate.span_id === event.span_id);
  const start = span?.start?.kind === "span_started" && span.start.span_kind === "tool.call"
    ? span.start
    : null;
  const previous = latestToolFact(bucket, event.scope.tool_call_id, event.span_id);
  if (start === null && previous === undefined) {
    return event.scope.tool_call_id === null ? bucket : markRefresh(bucket);
  }
  const detail = start === null ? null : toolDetail(start);
  const updated = appendToolFact(bucket, {
    phase: "finished",
    sequence: event.sequence,
    elapsed_ns: event.elapsed_ns,
    scope: event.scope,
    tool_call_id: event.scope.tool_call_id ?? previous?.tool_call_id ?? start?.scope.tool_call_id ?? null,
    span_id: event.span_id,
    title: previous?.title ?? detail?.title ?? null,
    tool_kind: previous?.tool_kind ?? detail?.tool_kind ?? null,
    status: event.outcome === "completed"
      ? "completed"
      : event.outcome === "failed"
        ? "failed"
        : null,
    outcome: event.outcome,
    error_code: event.error_code ?? previous?.error_code ?? detail?.error_code ?? null,
  });
  return start === null ? markRefresh(updated) : updated;
}

const RESULT_KINDS: readonly ProjectedResultFact["result_kind"][] = [
  "result.submitted",
  "result.rejected",
  "result.repair_requested",
  "result.accepted",
  "result.missing",
];

function isResultKind(kind: string): kind is ProjectedResultFact["result_kind"] {
  return RESULT_KINDS.includes(kind as ProjectedResultFact["result_kind"]);
}

function projectResult(
  bucket: ProjectionBucket<ProjectedResultFact>,
  event: DiagnosticEvent,
): ProjectionBucket<ProjectedResultFact> {
  if (event.kind !== "instant_occurred" || !isResultKind(event.instant_kind)) {
    return bucket;
  }
  const detail = event.detail as unknown as Pick<ProjectedResultFact, "issue" | "error_code">;
  return upsertBounded(
    bucket,
    {
      result_kind: event.instant_kind,
      sequence: event.sequence,
      elapsed_ns: event.elapsed_ns,
      scope: event.scope,
      act_id: event.scope.act_id,
      containing_span_id: event.containing_span_id,
      issue: detail.issue,
      error_code: detail.error_code,
    },
    RESULT_FACT_CAPACITY,
    (candidate) => candidate.sequence,
    (candidate) => candidate.sequence,
  );
}

function projectGap(bucket: GapProjection, event: DiagnosticEvent): GapProjection {
  if (event.kind !== "observation_gap") {
    return bucket;
  }
  const updated = upsertBounded(
    bucket,
    event,
    GAP_CAPACITY,
    (gap) => gap.sequence,
    (gap) => gap.sequence,
  );
  return {
    ...updated,
    declared_dropped_count: bucket.declared_dropped_count
      + (event.dropped_count === null ? 0n : BigInt(event.dropped_count)),
    has_unknown_dropped_count: bucket.has_unknown_dropped_count || event.dropped_count === null,
  };
}

function gapAffects(event: DiagnosticEvent, kinds: readonly DiagnosticEvent["kind"][]): boolean {
  return event.kind === "observation_gap"
    && (event.affected_kind === null || kinds.includes(event.affected_kind));
}

function projectEvent(projection: LiveProjection, event: DiagnosticEvent): LiveProjection {
  let spans = projectSpan(projection.spans, event);
  let messages = projectMessage(projection.messages, event);
  let counters = projectCounter(projection.counters, event);
  let contextUsage = projectContextUsage(projection.context_usage, event);
  let actUsage = projectActUsage(projection.act_usage, event);
  let tools = projectTool(projection.tools, spans, event);
  let results = projectResult(projection.results, event);
  if (gapAffects(event, ["span_started", "span_finished", "custom_span_started", "custom_span_finished"])) {
    spans = markRefresh(spans);
  }
  if (gapAffects(event, ["agent_message_delta", "agent_message_completed"])) {
    messages = markRefresh(messages);
  }
  if (gapAffects(event, ["counter_sampled", "custom_counter_sampled"])) {
    counters = markRefresh(counters);
  }
  if (gapAffects(event, ["context_usage_sampled"])) {
    contextUsage = markRefresh(contextUsage);
  }
  if (gapAffects(event, ["act_token_usage_finalized"])) {
    actUsage = markRefresh(actUsage);
  }
  if (gapAffects(event, ["span_started", "span_finished", "instant_occurred"])) {
    tools = markRefresh(tools);
  }
  if (gapAffects(event, ["instant_occurred"])) {
    results = markRefresh(results);
  }
  return {
    spans,
    messages,
    counters,
    context_usage: contextUsage,
    act_usage: actUsage,
    tools,
    results,
    gaps: projectGap(projection.gaps, event),
  };
}

function withPauseWatermark(pause: PauseState, watermark: U64String): PauseState {
  if (!pause.paused || pause.paused_at === null) {
    return pause;
  }
  const unseen = BigInt(watermark) - BigInt(pause.paused_at);
  return { ...pause, unseen_count: unseen > 0n ? unseen : 0n };
}

function acceptedEvent(state: DiagnosticState, event: DiagnosticEvent): DiagnosticState {
  const projection = projectEvent(state.live.projection, event);
  const watermark = compareU64(event.sequence, state.cursor.committed_watermark) > 0
    ? event.sequence
    : state.cursor.committed_watermark;
  return {
    ...state,
    cursor: {
      delivered_through: event.sequence,
      committed_watermark: watermark,
    },
    delivery_issue: null,
    live: appendLiveEvent(state.live, event, projection),
    queries: invalidateQueries(state.queries, event),
    pause: withPauseWatermark(state.pause, watermark),
  };
}

function projectionLostAfter(projection: LiveProjection, sequence: U64String): boolean {
  return [
    projection.spans,
    projection.messages,
    projection.counters,
    projection.context_usage,
    projection.act_usage,
    projection.tools,
    projection.results,
    projection.gaps,
  ].some((bucket) => bucket.dropped_through !== null && compareU64(bucket.dropped_through, sequence) > 0);
}

function resume(state: DiagnosticState): DiagnosticState {
  if (!state.pause.paused || state.pause.paused_at === null) {
    return state;
  }
  const after = state.pause.paused_at;
  const through = state.cursor.committed_watermark;
  const rawLoss = state.live.dropped_through !== null
    && compareU64(state.live.dropped_through, after) > 0;
  const deliveryLag = compareU64(state.cursor.delivered_through, through) < 0;
  const needsServerRange = compareU64(through, after) > 0
    && (rawLoss || deliveryLag || projectionLostAfter(state.live.projection, after));
  return {
    ...state,
    pause: {
      paused: false,
      paused_at: null,
      unseen_count: 0n,
      resume_request: needsServerRange
        ? { kind: "server_range", after_sequence: after, through_sequence: through }
        : null,
      frozen_live: null,
    },
  };
}

function receiveEvent(state: DiagnosticState, event: DiagnosticEvent): DiagnosticState {
  if (event.run_id !== state.run_id) {
    return {
      ...state,
      delivery_issue: {
        kind: "cross_run",
        expected_run_id: state.run_id,
        received_run_id: event.run_id,
        received_sequence: event.sequence,
      },
    };
  }
  if (compareU64(event.sequence, state.cursor.delivered_through) <= 0) {
    return state;
  }
  const expected = BigInt(state.cursor.delivered_through) + 1n;
  if (BigInt(event.sequence) !== expected) {
    const watermark = compareU64(event.sequence, state.cursor.committed_watermark) > 0
      ? event.sequence
      : state.cursor.committed_watermark;
    return {
      ...state,
      cursor: { ...state.cursor, committed_watermark: watermark },
      delivery_issue: {
        kind: "non_contiguous",
        expected_sequence: String(expected) as U64String,
        received_sequence: event.sequence,
      },
      queries: invalidateAllQueries(state.queries, watermark),
      pause: withPauseWatermark(state.pause, watermark),
    };
  }
  return acceptedEvent(state, event);
}

function staleAt(result: CachedQueryResult, sequence: U64String): CachedQueryResult {
  return {
    ...result,
    stale: true,
    invalidated_through: result.invalidated_through === null
      || compareU64(sequence, result.invalidated_through) > 0
      ? sequence
      : result.invalidated_through,
  };
}

function cacheQueryForCurrentState(
  state: DiagnosticState,
  result: CachedQueryResult,
): DiagnosticState["queries"] {
  let candidate = result;
  const captured = result.captured_through;
  const historyUnknown = compareU64(state.live.base_through, captured) > 0
    || (state.live.dropped_through !== null
      && compareU64(state.live.dropped_through, captured) > 0)
    || compareU64(state.cursor.delivered_through, state.cursor.committed_watermark) < 0;
  if (historyUnknown && compareU64(state.cursor.committed_watermark, captured) > 0) {
    candidate = staleAt(candidate, state.cursor.committed_watermark);
  } else {
    for (const event of state.live.events) {
      if (
        compareU64(event.sequence, captured) > 0
        && queryDependsOnEvent(result.dependency, event)
      ) {
        candidate = staleAt(candidate, event.sequence);
      }
    }
  }
  return cacheQueryResult(state.queries, candidate);
}

export function reduceDiagnosticState(
  state: DiagnosticState,
  action: DiagnosticStateAction,
): DiagnosticState {
  switch (action.type) {
    case "event_received":
      return receiveEvent(state, action.event);
    case "watermark_observed": {
      if (compareU64(action.through_sequence, state.cursor.committed_watermark) <= 0) {
        return state;
      }
      return {
        ...state,
        cursor: { ...state.cursor, committed_watermark: action.through_sequence },
        queries: invalidateAllQueries(state.queries, action.through_sequence),
        pause: withPauseWatermark(state.pause, action.through_sequence),
      };
    }
    case "pause":
      if (state.pause.paused) {
        return state;
      }
      return {
        ...state,
        pause: {
          paused: true,
          paused_at: state.cursor.delivered_through,
          unseen_count: BigInt(state.cursor.committed_watermark) - BigInt(state.cursor.delivered_through),
          resume_request: null,
          frozen_live: state.live,
        },
      };
    case "resume":
      return resume(state);
    case "resume_request_consumed":
      return state.pause.resume_request === null
        ? state
        : { ...state, pause: { ...state.pause, resume_request: null } };
    case "window_activated":
      if (action.window.run_id !== state.run_id) {
        return {
          ...state,
          delivery_issue: {
            kind: "cross_run",
            expected_run_id: state.run_id,
            received_run_id: action.window.run_id,
            received_sequence: action.window.captured_through,
          },
        };
      }
      return { ...state, windows: activateWindow(state.windows, action.window) };
    case "query_cached":
      return { ...state, queries: cacheQueryForCurrentState(state, action.result) };
    case "select":
      return { ...state, presentation: select(state.presentation, action.selection) };
    case "pin_detail":
      return { ...state, presentation: pinDetail(state.presentation, action.selection) };
    case "toggle_expanded":
      return { ...state, presentation: toggleExpanded(state.presentation, action.id) };
    case "filters_set":
      return { ...state, presentation: setFilters(state.presentation, action.filters) };
    case "viewport_set":
      return { ...state, presentation: setViewport(state.presentation, action.viewport) };
    case "follow_live_set":
      return { ...state, presentation: setFollowLive(state.presentation, action.follow_live) };
    case "zoom_set":
      return { ...state, presentation: setZoom(state.presentation, action.zoom) };
  }
}

export function presentedLiveEdge(state: DiagnosticState): LiveEdgeState {
  return state.pause.paused && state.pause.frozen_live !== null
    ? state.pause.frozen_live
    : state.live;
}
