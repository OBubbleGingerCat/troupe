import { describe, expect, it } from "vitest";

import { decodeCanonicalUuid, decodeU64 } from "../../src/protocol/decimal.ts";
import {
  type DiagnosticEvent,
  type DiagnosticScope,
  decodeDiagnosticEvent,
} from "../../src/protocol/event.ts";
import {
  type EventsResponse,
  type SnapshotResponse,
  type UsageSnapshot,
  decodeSnapshotResponse,
} from "../../src/protocol/http.ts";
import {
  createDiagnosticState,
  createDiagnosticStateFromSnapshot,
  hydrateDiagnosticStateFromSnapshot,
  presentedLiveEdge,
  reduceDiagnosticState,
  selectUsagePanelFacts,
} from "../../src/state/reducer.ts";
import { cacheQueryResult, queryDependsOnEvent } from "../../src/state/queries.ts";
import {
  ACT_USAGE_CAPACITY,
  CONTEXT_USAGE_CAPACITY,
  COUNTER_SERIES_CAPACITY,
  GAP_CAPACITY,
  MESSAGE_CAPACITY,
  SPAN_CAPACITY,
  USAGE_SCOPE_AGGREGATE_CAPACITY,
  VISIBLE_WINDOW_EVENT_CAPACITY,
} from "../../src/state/model.ts";
import {
  createPresentationState,
  eventReference,
  hierarchyScope,
  hierarchyScopeReference,
  messageReference,
  scopeFromReference,
  scopeReference,
  spanReference,
} from "../../src/state/selection.ts";
import { loadHttpFixture } from "../support/diagnostic-fixtures.ts";


const RUN_ID = decodeCanonicalUuid("12345678-1234-4234-9234-123456789abc");
const OTHER_RUN_ID = decodeCanonicalUuid("aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa");
const SCOPE: DiagnosticScope = {
  scene_id: "scene-1",
  actor_id: "actor-1",
  cue_id: "cue-1",
  effect_id: null,
  act_id: "act-1",
  tool_call_id: null,
  session_generation: decodeU64("1"),
};
const TOOL_SCOPE: DiagnosticScope = { ...SCOPE, tool_call_id: "tool-1" };

function loadSnapshotFixture(): unknown {
  const fixture = structuredClone(loadHttpFixture("snapshot-v1.json")) as {
    state: { usage: Record<string, unknown> };
  };
  if (!Object.prototype.hasOwnProperty.call(fixture.state.usage, "contexts")) {
    fixture.state.usage.contexts = [];
  }
  return fixture;
}

function loadMaterializedSnapshotFixture(): unknown {
  const fixture = loadSnapshotFixture() as Record<string, unknown>;
  const state = fixture.state as Record<string, unknown>;
  fixture.watermark_sequence = "9";
  state.through_sequence = "9";
  state.through_elapsed_ns = "90";
  for (const name of ["spans", "messages", "plans", "counters", "usage"]) {
    const child = state[name] as Record<string, unknown>;
    child.through_sequence = "9";
    child.through_elapsed_ns = "90";
  }
  const usage = state.usage as Record<string, unknown>;
  const terminalUsage = (usage.usages as Record<string, unknown>[])[0]!;
  const scope = terminalUsage.scope as Record<string, unknown>;
  usage.contexts = [{
    run_id: RUN_ID,
    scope,
    sequence: "2",
    elapsed_ns: "20",
    caused_by: [],
    context_used_tokens: "500",
    context_window_tokens: "1000",
    cumulative_cost_amount: "0.25",
    cumulative_cost_currency: "USD",
    sample_origin: "provider",
    observed_elapsed_ns: null,
  }];
  (state.spans as Record<string, unknown>).spans = [{
    run_id: RUN_ID,
    span_id: "3",
    started_at_ns: "30",
    scope: { ...scope, tool_call_id: "tool-1" },
    parent_span_id: null,
    started_caused_by: [],
    definition: {
      family: "built_in",
      detail: {
        span_kind: "tool.call",
        detail: {
          title: "Read source",
          tool_kind: "read",
          status: "in_progress",
          error_code: null,
        },
      },
    },
    completion: {
      finish_sequence: "4",
      finished_at_ns: "40",
      outcome: "completed",
      error_code: null,
      caused_by: [{ source_sequence: "3", relation: "follows_from" }],
    },
  }];
  (state.messages as Record<string, unknown>).messages = [{
    run_id: RUN_ID,
    message_id: "message-1",
    scope,
    first_sequence: "5",
    first_elapsed_ns: "50",
    latest_sequence: "6",
    latest_elapsed_ns: "60",
    source_message_id: "provider-message-1",
    text: "hello",
    completion: {
      sequence: "6",
      elapsed_ns: "60",
      utf8_bytes: "5",
      unicode_scalar_count: "5",
      truncated: true,
      caused_by: [{ source_sequence: "5", relation: "follows_from" }],
    },
  }];
  (state.plans as Record<string, unknown>).plans = [{
    run_id: RUN_ID,
    scope,
    sequence: "7",
    elapsed_ns: "70",
    entries: [{ content: "Inspect source", priority: "high", status: "in_progress" }],
    truncated: true,
    caused_by: [],
  }];
  const counter = ((state.counters as Record<string, unknown>).series as Record<string, unknown>[])[0]!;
  counter.sequence = "8";
  counter.elapsed_ns = "80";
  state.gaps = [{
    schema_version: 1,
    run_id: RUN_ID,
    sequence: "9",
    elapsed_ns: "90",
    scope,
    caused_by: [],
    producer: "acp-normalizer",
    component: "message-stream",
    reason: "provider_sequence_gap",
    dropped_count: "2",
    affected_elapsed: { start_ns: "50", end_ns: "60" },
    affected_kind: "agent_message_delta",
    affected_scope: scope,
  }];
  state.truncations = [
    { source: "agent_message", sequence: "6", scope, message_id: "message-1" },
    { source: "agent_plan", sequence: "7", scope },
  ];
  return fixture;
}

function event(sequence: number, fields: Record<string, unknown>): DiagnosticEvent {
  return decodeDiagnosticEvent({
    schema_version: 1,
    run_id: RUN_ID,
    sequence: String(sequence),
    elapsed_ns: String(sequence * 10),
    scope: SCOPE,
    caused_by: [],
    ...fields,
  });
}

function ingest(state: ReturnType<typeof createDiagnosticState>, events: readonly DiagnosticEvent[]) {
  return events.reduce(
    (current, item) => reduceDiagnosticState(current, { type: "event_received", event: item }),
    state,
  );
}

function finiteSuffix(
  snapshot: SnapshotResponse,
  after: number,
  events: readonly DiagnosticEvent[],
  capturedWatermark = snapshot.watermark_sequence,
): EventsResponse {
  return {
    api_schema_version: 1,
    run_id: snapshot.run_id,
    captured_watermark: capturedWatermark,
    events,
    next_after: null,
  };
}

function snapshotAtThrough(snapshot: SnapshotResponse, through: number): SnapshotResponse {
  const throughSequence = decodeU64(String(through));
  const throughElapsedNs = decodeU64(String(through * 10));
  return {
    ...snapshot,
    watermark_sequence: throughSequence,
    state: {
      ...snapshot.state,
      through_sequence: throughSequence,
      through_elapsed_ns: throughElapsedNs,
      spans: { ...snapshot.state.spans, through_sequence: throughSequence, through_elapsed_ns: throughElapsedNs },
      messages: { ...snapshot.state.messages, through_sequence: throughSequence, through_elapsed_ns: throughElapsedNs },
      plans: { ...snapshot.state.plans, through_sequence: throughSequence, through_elapsed_ns: throughElapsedNs },
      counters: { ...snapshot.state.counters, through_sequence: throughSequence, through_elapsed_ns: throughElapsedNs },
      usage: { ...snapshot.state.usage, through_sequence: throughSequence, through_elapsed_ns: throughElapsedNs },
    },
  };
}

describe("diagnostic state reducer", () => {
  it("retains bounded validated usage snapshots and invalidates aggregates without recomputing", () => {
    const response = decodeSnapshotResponse(loadSnapshotFixture());
    let state = createDiagnosticState(RUN_ID, response.watermark_sequence);
    state = reduceDiagnosticState(state, {
      type: "usage_snapshot_received",
      snapshot: response.state.usage,
    });

    expect(selectUsagePanelFacts(state)).toMatchObject({
      needs_server_refresh: false,
      usages: [{ act_key: "act-1" }],
      aggregates: [{ scope_kind: "run", scope_label: "Run" }],
    });

    state = reduceDiagnosticState(state, {
      type: "select",
      selection: hierarchyScopeReference(SCOPE, "actor_id"),
    });
    expect(selectUsagePanelFacts(state).aggregates.map((aggregate) => aggregate.scope_kind)).toEqual([
      "run",
      "scene",
      "actor",
    ]);

    state = reduceDiagnosticState(state, {
      type: "event_received",
      event: event(3, {
        kind: "act_token_usage_finalized",
        scope: { ...SCOPE, cue_id: "cue-2", act_id: "act-2" },
        availability: "partial",
        source: "acp.prompt_response.usage",
        unavailable_reason: null,
        provider_total_tokens: null,
        input_tokens: "9",
        output_tokens: null,
        thought_tokens: null,
        cached_read_tokens: null,
        cached_write_tokens: null,
      }),
    });
    const afterLiveUsage = selectUsagePanelFacts(state);
    expect(afterLiveUsage.usages.map((usage) => usage.act_key)).toEqual(["act-1", "act-2"]);
    expect(afterLiveUsage.aggregates[0]?.aggregate.finalized_acts).toBe("1");
    expect(afterLiveUsage.needs_server_refresh).toBe(true);

    const beforeOldSnapshot = state;
    state = reduceDiagnosticState(state, {
      type: "usage_snapshot_received",
      snapshot: response.state.usage,
    });
    expect(state).toBe(beforeOldSnapshot);

    state = reduceDiagnosticState(state, {
      type: "select",
      selection: hierarchyScopeReference({ ...SCOPE, actor_id: "actor-other" }, "actor_id"),
    });
    expect(selectUsagePanelFacts(state).aggregates.map((aggregate) => aggregate.scope_kind)).toEqual([
      "run",
      "scene",
    ]);
    expect(selectUsagePanelFacts(state).usages).toEqual([]);
  });

  it("bounds retained usage records and scoped aggregates", () => {
    const response = decodeSnapshotResponse(loadSnapshotFixture());
    const baseUsage = response.state.usage.usages[0]!;
    const baseScoped = response.state.usage.scoped_aggregates[0]!;
    const usageCount = ACT_USAGE_CAPACITY + 1;
    const scopeCount = USAGE_SCOPE_AGGREGATE_CAPACITY + 1;
    const snapshot: UsageSnapshot = {
      ...response.state.usage,
      through_sequence: decodeU64(String(usageCount)),
      through_elapsed_ns: decodeU64(String(usageCount * 10)),
      usages: Array.from({ length: usageCount }, (_, index) => {
        const ordinal = index + 1;
        const actId = `act-${ordinal}`;
        return {
          act_id: actId,
          event: {
            ...baseUsage.event,
            sequence: decodeU64(String(ordinal)),
            elapsed_ns: decodeU64(String(ordinal * 10)),
            scope: { ...baseUsage.event.scope, act_id: actId },
          },
        };
      }),
      scoped_aggregates: Array.from({ length: scopeCount }, (_, index) => ({
        scope: { ...baseScoped.scope, scene_id: `scene-${index + 1}` },
        aggregate: baseScoped.aggregate,
      })),
    };
    let state = createDiagnosticState(
      RUN_ID,
      snapshot.through_sequence,
      snapshot.through_elapsed_ns,
    );
    state = reduceDiagnosticState(state, { type: "usage_snapshot_received", snapshot });

    expect(state.usage_snapshot?.usages).toHaveLength(ACT_USAGE_CAPACITY);
    expect(state.usage_snapshot?.scoped_aggregates).toHaveLength(USAGE_SCOPE_AGGREGATE_CAPACITY);
    expect(state.usage_snapshot?.truncated).toBe(true);
    expect(selectUsagePanelFacts(state).needs_server_refresh).toBe(true);
  });

  it("atomically restores typed snapshot facts without synthesizing raw history", () => {
    const snapshot = decodeSnapshotResponse(loadMaterializedSnapshotFixture());
    const initialized = createDiagnosticStateFromSnapshot(snapshot);
    expect(initialized.cursor).toEqual({ delivered_through: "9", committed_watermark: "9" });

    let previous = ingest(createDiagnosticState(RUN_ID, decodeU64("0")), [
      event(1, { kind: "counter_sampled", counter_kind: "cue.active", value: "1" }),
    ]);
    previous = reduceDiagnosticState(previous, {
      type: "select",
      selection: spanReference(decodeU64("3")),
    });
    previous = {
      ...previous,
      queries: cacheQueryResult(previous.queries, {
        key: "old-query",
        captured_through: decodeU64("1"),
        value: { stale: true },
        stale: false,
        invalidated_through: null,
        dependency: { event_kinds: null, scope: null, elapsed_range: null },
      }),
    };
    previous = reduceDiagnosticState(previous, { type: "pause" });
    const frozen = previous.pause.frozen_live;

    const state = reduceDiagnosticState(previous, { type: "snapshot_received", snapshot });
    expect(state.cursor).toEqual({ delivered_through: "9", committed_watermark: "9" });
    expect(state.delivery_issue).toBeNull();
    expect(state.live.base_through).toBe("9");
    expect(state.live.observed_elapsed_ns).toBe("90");
    expect(state.live.events).toEqual([]);
    expect(state.queries.entries.size).toBe(0);
    expect(state.presentation.selection).toEqual(spanReference(decodeU64("3")));
    expect(state.pause).toMatchObject({ paused: true, paused_at: "1", unseen_count: 8n });
    expect(state.pause.frozen_live).toBe(frozen);

    expect(state.live.projection.spans.items[0]).toMatchObject({
      span_id: "3",
      start: { kind: "span_started", span_kind: "tool.call" },
      finish: { kind: "span_finished", sequence: "4", outcome: "completed" },
    });
    expect(state.live.projection.tools.items.map((fact) => fact.phase)).toEqual([
      "started",
      "finished",
    ]);
    expect(state.live.projection.messages.items[0]).toMatchObject({
      message_id: "message-1",
      text: "hello",
      text_complete_from_start: false,
      text_truncated_before: true,
      completion: { sequence: "6", truncated: true },
    });
    expect(state.live.projection.messages.dropped_through).toBe("6");
    expect(state.live.projection.messages.needs_server_refresh).toBe(true);
    expect(state.live.projection.counters.items[0]?.event.sequence).toBe("8");
    expect(state.live.projection.context_usage.items[0]?.event).toMatchObject({
      sequence: "2",
      context_used_tokens: "500",
    });
    expect(state.usage_snapshot?.usages[0]?.event.sequence).toBe("1");
    expect(state.live.projection.gaps.items[0]?.sequence).toBe("9");
    expect(state.live.projection.gaps.declared_dropped_count).toBe(2n);
    expect("plans" in state.live.projection).toBe(false);
    expect(presentedLiveEdge(state)).toBe(frozen);
  });

  it("atomically hydrates the EventTable and only missing instant projections", () => {
    const snapshot = decodeSnapshotResponse(loadMaterializedSnapshotFixture());
    const rawEvents = Array.from({ length: 9 }, (_, index) => {
      const sequence = index + 1;
      if (sequence === 2) {
        return event(sequence, {
          kind: "instant_occurred",
          scope: TOOL_SCOPE,
          instant_kind: "tool.updated",
          detail: {
            title: "Read updated source",
            tool_kind: "read",
            status: "in_progress",
            error_code: null,
          },
          containing_span_id: "3",
        });
      }
      if (sequence === 7) {
        return event(sequence, {
          kind: "instant_occurred",
          instant_kind: "result.submitted",
          detail: { issue: null, error_code: null },
          containing_span_id: null,
        });
      }
      return event(sequence, {
        kind: "counter_sampled",
        counter_kind: "cue.active",
        value: String(100 + sequence),
      });
    });
    const suffix = finiteSuffix(snapshot, 0, rawEvents, decodeU64("12"));

    const state = hydrateDiagnosticStateFromSnapshot({
      snapshot,
      suffix,
      after: decodeU64("0"),
    });

    expect(state.cursor).toEqual({ delivered_through: "9", committed_watermark: "9" });
    expect(state.windows.visible).toMatchObject({
      id: `bootstrap:${RUN_ID}:0:9`,
      captured_through: "9",
      events: rawEvents,
    });
    expect(state.live.events).toEqual([]);
    expect(state.live.projection.spans.items).toHaveLength(1);
    expect(state.live.projection.messages.items).toHaveLength(1);
    expect(state.live.projection.counters.items[0]?.event.sequence).toBe("8");
    expect(state.live.projection.gaps.items).toHaveLength(1);
    expect(state.live.projection.tools.items.map((fact) => fact.sequence)).toEqual([
      "2",
      "3",
      "4",
    ]);
    expect(state.live.projection.tools.needs_server_refresh).toBe(false);
    expect(state.live.projection.results.items.map((fact) => fact.sequence)).toEqual(["7"]);
    expect(state.live.projection.results.needs_server_refresh).toBe(false);
  });

  it("marks instant projections incomplete when bootstrap history starts after zero", () => {
    const snapshot = decodeSnapshotResponse(loadMaterializedSnapshotFixture());
    const rawEvents = [6, 7, 8, 9].map((sequence) => sequence === 7
      ? event(sequence, {
        kind: "instant_occurred",
        instant_kind: "result.rejected",
        detail: { issue: { code: "invalid", path: "/value" }, error_code: "invalid_result" },
        containing_span_id: null,
      })
      : event(sequence, {
        kind: "instant_occurred",
        scope: TOOL_SCOPE,
        instant_kind: "tool.updated",
        detail: {
          title: `Tool update ${sequence}`,
          tool_kind: "read",
          status: "in_progress",
          error_code: null,
        },
        containing_span_id: "3",
      }));

    const state = hydrateDiagnosticStateFromSnapshot({
      snapshot,
      suffix: finiteSuffix(snapshot, 5, rawEvents),
      after: decodeU64("5"),
    });

    expect(state.windows.visible?.events.map((candidate) => candidate.sequence)).toEqual([
      "6",
      "7",
      "8",
      "9",
    ]);
    expect(state.live.projection.tools).toMatchObject({
      dropped_through: "5",
      needs_server_refresh: true,
    });
    expect(state.live.projection.results).toMatchObject({
      dropped_through: "5",
      needs_server_refresh: true,
    });
  });

  it("rejects malformed bootstrap suffixes before changing prior state", () => {
    const snapshot = decodeSnapshotResponse(loadMaterializedSnapshotFixture());
    const previous = ingest(createDiagnosticState(RUN_ID, decodeU64("0")), [
      event(1, { kind: "counter_sampled", counter_kind: "cue.active", value: "1" }),
    ]);
    const exact = Array.from({ length: 9 }, (_, index) => event(index + 1, {
      kind: "counter_sampled",
      counter_kind: "cue.active",
      value: String(index + 1),
    }));
    const malformed = [
      finiteSuffix(snapshot, 0, exact.slice(0, 8)),
      finiteSuffix(snapshot, 0, [...exact.slice(0, 4), exact[5]!, ...exact.slice(5)]),
      finiteSuffix(snapshot, 0, exact, decodeU64("8")),
      { ...finiteSuffix(snapshot, 0, exact), next_after: decodeU64("9") },
    ];

    for (const suffix of malformed) {
      expect(() => hydrateDiagnosticStateFromSnapshot({
        snapshot,
        suffix,
        after: decodeU64("0"),
        previous,
      })).toThrow(RangeError);
    }
    expect(previous.cursor.delivered_through).toBe("1");
    expect(previous.live.events).toHaveLength(1);
  });

  it("accepts the exact visible suffix capacity and rejects a larger requested range", () => {
    const base = decodeSnapshotResponse(loadMaterializedSnapshotFixture());
    const snapshot = snapshotAtThrough(base, VISIBLE_WINDOW_EVENT_CAPACITY);
    const rawEvents = Array.from(
      { length: VISIBLE_WINDOW_EVENT_CAPACITY },
      (_, index) => event(index + 1, {
        kind: "counter_sampled",
        counter_kind: "cue.active",
        value: String(index + 1),
      }),
    );
    const accepted = hydrateDiagnosticStateFromSnapshot({
      snapshot,
      suffix: finiteSuffix(snapshot, 0, rawEvents),
      after: decodeU64("0"),
    });
    expect(accepted.windows.visible?.events).toHaveLength(VISIBLE_WINDOW_EVENT_CAPACITY);

    const oversized = snapshotAtThrough(base, VISIBLE_WINDOW_EVENT_CAPACITY + 1);
    expect(() => hydrateDiagnosticStateFromSnapshot({
      snapshot: oversized,
      suffix: finiteSuffix(oversized, 0, []),
      after: decodeU64("0"),
    })).toThrow("event suffix exceeds the visible window capacity");
  });

  it("records snapshot capacity loss while retaining complete gap totals", () => {
    const snapshot = decodeSnapshotResponse(loadMaterializedSnapshotFixture());
    const through = decodeU64("5000");
    const elapsed = decodeU64("50000");
    const baseSpan = snapshot.state.spans.spans[0]!;
    const baseMessage = snapshot.state.messages.messages[0]!;
    const baseCounter = snapshot.state.counters.series[0]!;
    const baseContext = snapshot.state.usage.contexts[0]!;
    const baseGap = snapshot.state.gaps[0]!;
    const spans = Array.from({ length: SPAN_CAPACITY + 1 }, (_, index): typeof baseSpan => {
      const ordinal = index + 1;
      return {
        ...baseSpan,
        span_id: decodeU64(String(ordinal)),
        started_at_ns: decodeU64(String(ordinal * 10)),
        scope: { ...baseSpan.scope, tool_call_id: null },
        definition: {
          family: "built_in",
          detail: {
            span_kind: "act.lifecycle",
            detail: { provider: "codex", effective_model: "gpt-5", effective_effort: "high" },
          },
        },
        completion: null,
      };
    });
    const messages = Array.from(
      { length: MESSAGE_CAPACITY + 1 },
      (_, index): typeof baseMessage => {
        const ordinal = index + 1;
        const sequence = decodeU64(String(3000 + ordinal));
        return {
          ...baseMessage,
          message_id: `message-${ordinal}`,
          first_sequence: sequence,
          first_elapsed_ns: decodeU64(String((3000 + ordinal) * 10)),
          latest_sequence: sequence,
          latest_elapsed_ns: decodeU64(String((3000 + ordinal) * 10)),
          completion: baseMessage.completion === null ? null : {
            ...baseMessage.completion,
            sequence,
            elapsed_ns: decodeU64(String((3000 + ordinal) * 10)),
            truncated: false,
          },
        };
      },
    );
    const counters = Array.from(
      { length: COUNTER_SERIES_CAPACITY + 1 },
      (_, index): typeof baseCounter => {
        const ordinal = index + 1;
        return {
          ...baseCounter,
          series_key: `typed-series-${ordinal}`,
          identity: {
            ...baseCounter.identity,
            scope: { ...baseCounter.identity.scope, cue_id: `counter-cue-${ordinal}` },
          },
          sequence: decodeU64(String(1000 + ordinal)),
          elapsed_ns: decodeU64(String((1000 + ordinal) * 10)),
        };
      },
    );
    const contexts = Array.from(
      { length: CONTEXT_USAGE_CAPACITY + 1 },
      (_, index): typeof baseContext => {
        const ordinal = index + 1;
        return {
          ...baseContext,
          scope: { ...baseContext.scope, cue_id: `context-cue-${ordinal}` },
          sequence: decodeU64(String(2000 + ordinal)),
          elapsed_ns: decodeU64(String((2000 + ordinal) * 10)),
        };
      },
    );
    const gaps = Array.from({ length: GAP_CAPACITY + 1 }, (_, index): typeof baseGap => {
      const ordinal = index + 1;
      return {
        ...baseGap,
        sequence: decodeU64(String(4000 + ordinal)),
        elapsed_ns: decodeU64(String((4000 + ordinal) * 10)),
      };
    });
    const boundedSnapshot: SnapshotResponse = {
      ...snapshot,
      watermark_sequence: through,
      state: {
        ...snapshot.state,
        through_sequence: through,
        through_elapsed_ns: elapsed,
        spans: {
          ...snapshot.state.spans,
          through_sequence: through,
          through_elapsed_ns: elapsed,
          spans,
        },
        messages: {
          ...snapshot.state.messages,
          through_sequence: through,
          through_elapsed_ns: elapsed,
          messages,
        },
        plans: {
          ...snapshot.state.plans,
          through_sequence: through,
          through_elapsed_ns: elapsed,
          plans: snapshot.state.plans.plans.map((plan) => ({ ...plan, truncated: false })),
        },
        counters: {
          ...snapshot.state.counters,
          through_sequence: through,
          through_elapsed_ns: elapsed,
          series: counters,
        },
        usage: {
          ...snapshot.state.usage,
          through_sequence: through,
          through_elapsed_ns: elapsed,
          contexts,
        },
        gaps,
        truncations: [],
      },
    };

    const state = createDiagnosticStateFromSnapshot(boundedSnapshot);
    expect(state.live.projection.spans.items).toHaveLength(SPAN_CAPACITY);
    expect(state.live.projection.spans.dropped_through).toBe("1");
    expect(state.live.projection.spans.needs_server_refresh).toBe(true);
    expect(state.live.projection.messages.items).toHaveLength(MESSAGE_CAPACITY);
    expect(state.live.projection.messages.dropped_through).toBe("3001");
    expect(state.live.projection.messages.needs_server_refresh).toBe(true);
    expect(state.live.projection.counters.items).toHaveLength(COUNTER_SERIES_CAPACITY);
    expect(state.live.projection.counters.dropped_through).toBe("1001");
    expect(state.live.projection.context_usage.items).toHaveLength(CONTEXT_USAGE_CAPACITY);
    expect(state.live.projection.context_usage.dropped_through).toBe("2001");
    expect(state.live.projection.gaps.items).toHaveLength(GAP_CAPACITY);
    expect(state.live.projection.gaps.dropped_through).toBe("4001");
    expect(state.live.projection.gaps.declared_dropped_count).toBe(
      BigInt(GAP_CAPACITY + 1) * 2n,
    );
  });

  it("shares one canonical event, span, message, and scope selection contract", () => {
    expect(eventReference(decodeU64("12"))).toEqual({ kind: "event", id: "12" });
    expect(spanReference(decodeU64("7"))).toEqual({ kind: "span", id: "7" });
    expect(messageReference("message-1")).toEqual({ kind: "message", id: "message-1" });
    expect(scopeFromReference(scopeReference(SCOPE))).toEqual(SCOPE);
    expect(scopeFromReference(hierarchyScopeReference(SCOPE, "actor_id"))).toEqual({
      scene_id: "scene-1",
      actor_id: "actor-1",
      cue_id: null,
      effect_id: null,
      act_id: null,
      tool_call_id: null,
      session_generation: null,
    });
    expect(hierarchyScope(SCOPE, "cue_id")).toEqual({
      scene_id: "scene-1",
      actor_id: "actor-1",
      cue_id: "cue-1",
      effect_id: null,
      act_id: null,
      tool_call_id: null,
      session_generation: null,
    });
    expect(scopeFromReference({ kind: "scope", id: "not-json" })).toBeNull();
    expect(scopeFromReference({ kind: "scope", id: "[null]" })).toBeNull();
  });

  it("projects span, message, tool, result, counter, usage, and gap facts", () => {
    const events = [
      event(1, {
        kind: "span_started",
        span_kind: "act.lifecycle",
        detail: { provider: "codex", effective_model: "gpt-5", effective_effort: "high" },
        parent_span_id: null,
      }),
      event(2, {
        kind: "agent_message_delta",
        message_id: "message-1",
        source_message_id: "provider-message-1",
        text_delta: "hello ",
      }),
      event(3, {
        kind: "agent_message_delta",
        message_id: "message-1",
        source_message_id: "provider-message-1",
        text_delta: "world",
      }),
      event(4, {
        kind: "agent_message_completed",
        message_id: "message-1",
        utf8_bytes: "11",
        unicode_scalar_count: "11",
        truncated: false,
      }),
      event(5, {
        kind: "counter_sampled",
        counter_kind: "agent.turn.active",
        value: "1",
      }),
      event(6, {
        kind: "context_usage_sampled",
        context_used_tokens: "900",
        context_window_tokens: "1000",
        cumulative_cost_amount: "0.25",
        cumulative_cost_currency: "USD",
        sample_origin: "provider",
        observed_elapsed_ns: null,
      }),
      event(7, {
        kind: "act_token_usage_finalized",
        availability: "available",
        source: "acp.prompt_response.usage",
        unavailable_reason: null,
        provider_total_tokens: "123456789012345678901234567890",
        input_tokens: "700",
        output_tokens: "200",
        thought_tokens: "50",
        cached_read_tokens: "20",
        cached_write_tokens: "0",
      }),
      event(8, {
        kind: "observation_gap",
        producer: "acp-normalizer",
        component: "message-stream",
        reason: "provider_sequence_gap",
        dropped_count: "2",
        affected_elapsed: { start_ns: "20", end_ns: "30" },
        affected_kind: "agent_message_delta",
        affected_scope: SCOPE,
      }),
      event(9, {
        kind: "span_started",
        scope: TOOL_SCOPE,
        span_kind: "tool.call",
        detail: {
          title: "Read source",
          tool_kind: "read",
          status: "in_progress",
          error_code: null,
        },
        parent_span_id: "1",
      }),
      event(10, {
        kind: "instant_occurred",
        scope: TOOL_SCOPE,
        instant_kind: "tool.updated",
        detail: {
          title: "Read source",
          tool_kind: "read",
          status: "completed",
          error_code: null,
        },
        containing_span_id: "9",
      }),
      event(11, {
        kind: "instant_occurred",
        instant_kind: "result.rejected",
        detail: {
          issue: { code: "out_of_range", path: "/score" },
          error_code: "invalid_result",
        },
        containing_span_id: "1",
      }),
      event(12, {
        kind: "span_finished",
        scope: TOOL_SCOPE,
        span_id: "9",
        outcome: "completed",
        error_code: null,
      }),
      event(13, {
        kind: "span_finished",
        span_id: "1",
        outcome: "completed",
        error_code: null,
      }),
    ];

    const state = ingest(createDiagnosticState(RUN_ID, decodeU64("0")), events);

    expect(state.cursor.delivered_through).toBe("13");
    expect(state.cursor.committed_watermark).toBe("13");
    expect(state.delivery_issue).toBeNull();
    expect(state.live.projection.spans.items[0]?.finish?.sequence).toBe("13");
    expect(state.live.projection.messages.items[0]).toMatchObject({
      message_id: "message-1",
      text: "hello world",
    });
    expect(state.live.projection.messages.items[0]?.completion?.sequence).toBe("4");
    expect(state.live.projection.counters.items[0]?.event.sequence).toBe("5");
    expect(state.live.projection.context_usage.items[0]?.event.context_used_tokens).toBe("900");
    expect(state.live.projection.act_usage.items[0]?.event.provider_total_tokens).toBe(
      "123456789012345678901234567890",
    );
    expect(state.live.projection.tools.items.map((fact) => fact.phase)).toEqual([
      "started",
      "updated",
      "finished",
    ]);
    expect(state.live.projection.tools.items[2]).toMatchObject({
      tool_call_id: "tool-1",
      span_id: "9",
      title: "Read source",
      tool_kind: "read",
      status: "completed",
      outcome: "completed",
    });
    expect(state.live.projection.results.items[0]).toMatchObject({
      result_kind: "result.rejected",
      act_id: "act-1",
      issue: { code: "out_of_range", path: "/score" },
      error_code: "invalid_result",
    });
    expect(state.live.projection.gaps.items[0]?.sequence).toBe("8");
    expect(state.live.projection.gaps.declared_dropped_count).toBe(2n);
  });

  it("marks tool and result projections for refresh after an instant gap", () => {
    const state = ingest(createDiagnosticState(RUN_ID, decodeU64("0")), [
      event(1, {
        kind: "observation_gap",
        producer: "acp-normalizer",
        component: "transcript",
        reason: "provider_sequence_gap",
        dropped_count: null,
        affected_elapsed: null,
        affected_kind: "instant_occurred",
        affected_scope: SCOPE,
      }),
    ]);

    expect(state.live.projection.tools.needs_server_refresh).toBe(true);
    expect(state.live.projection.results.needs_server_refresh).toBe(true);
  });

  it("is idempotent by run and sequence while requiring the next contiguous cursor", () => {
    const first = event(1, {
      kind: "counter_sampled",
      counter_kind: "cue.active",
      value: "1",
    });
    const initial = createDiagnosticState(RUN_ID, decodeU64("0"));
    const once = reduceDiagnosticState(initial, { type: "event_received", event: first });

    expect(reduceDiagnosticState(once, { type: "event_received", event: first })).toBe(once);

    const skipped = reduceDiagnosticState(once, {
      type: "event_received",
      event: event(3, {
        kind: "counter_sampled",
        counter_kind: "cue.active",
        value: "0",
      }),
    });
    expect(skipped.cursor.delivered_through).toBe("1");
    expect(skipped.cursor.committed_watermark).toBe("3");
    expect(skipped.delivery_issue).toEqual({
      kind: "non_contiguous",
      expected_sequence: "2",
      received_sequence: "3",
    });

    const crossRun = decodeDiagnosticEvent({ ...first, run_id: OTHER_RUN_ID, sequence: "2" });
    const rejected = reduceDiagnosticState(once, { type: "event_received", event: crossRun });
    expect(rejected.cursor).toBe(once.cursor);
    expect(rejected.live).toBe(once.live);
    expect(rejected.delivery_issue?.kind).toBe("cross_run");
  });

  it("keeps committed watermark separate and invalidates, rather than recomputes, queries", () => {
    let state = createDiagnosticState(RUN_ID, decodeU64("10"));
    state = reduceDiagnosticState(state, {
      type: "watermark_observed",
      through_sequence: decodeU64("12"),
    });
    expect(state.cursor).toEqual({ delivered_through: "10", committed_watermark: "12" });

    const query = {
      key: "usage:act-1",
      captured_through: decodeU64("10"),
      value: { provider_total_tokens: "9" },
      stale: false,
      invalidated_through: null,
      dependency: {
        event_kinds: ["act_token_usage_finalized"] as const,
        scope: SCOPE,
        elapsed_range: null,
      },
    };
    state = { ...state, queries: cacheQueryResult(state.queries, query) };
    state = reduceDiagnosticState(state, {
      type: "event_received",
      event: event(11, {
        kind: "act_token_usage_finalized",
        availability: "partial",
        source: "acp.prompt_response.usage",
        unavailable_reason: null,
        provider_total_tokens: null,
        input_tokens: "10",
        output_tokens: null,
        thought_tokens: null,
        cached_read_tokens: null,
        cached_write_tokens: null,
      }),
    });

    const cached = state.queries.entries.get("usage:act-1");
    expect(cached?.value).toEqual({ provider_total_tokens: "9" });
    expect(cached?.stale).toBe(true);
    expect(cached?.invalidated_through).toBe("11");
  });

  it("treats an unknown gap scope as a wildcard and uses half-open query ranges", () => {
    const dependency = {
      event_kinds: ["counter_sampled"] as const,
      scope: { ...SCOPE, actor_id: "actor-other" },
      elapsed_range: { start_ns: decodeU64("0"), end_ns: decodeU64("10") },
    };
    const boundaryEvent = event(1, {
      kind: "counter_sampled",
      counter_kind: "cue.active",
      value: "1",
    });
    const unknownScopeGap = event(2, {
      kind: "observation_gap",
      producer: "runtime",
      component: null,
      reason: "unknown-scope",
      dropped_count: null,
      affected_elapsed: null,
      affected_kind: "counter_sampled",
      affected_scope: null,
    });
    const boundaryGap = event(3, {
      kind: "observation_gap",
      producer: "runtime",
      component: null,
      reason: "right-boundary",
      dropped_count: null,
      affected_elapsed: { start_ns: "10", end_ns: "20" },
      affected_kind: "counter_sampled",
      affected_scope: null,
    });

    expect(queryDependsOnEvent(dependency, boundaryEvent)).toBe(false);
    expect(queryDependsOnEvent({ ...dependency, elapsed_range: null }, unknownScopeGap)).toBe(true);
    expect(queryDependsOnEvent(dependency, boundaryGap)).toBe(false);
  });

  it("rejects a late query response when retained events or delivery lag passed its capture", () => {
    let state = createDiagnosticState(RUN_ID, decodeU64("0"));
    state = reduceDiagnosticState(state, {
      type: "event_received",
      event: event(1, {
        kind: "act_token_usage_finalized",
        availability: "partial",
        source: "acp.prompt_response.usage",
        unavailable_reason: null,
        provider_total_tokens: null,
        input_tokens: "10",
        output_tokens: null,
        thought_tokens: null,
        cached_read_tokens: null,
        cached_write_tokens: null,
      }),
    });
    const lateResult = {
      key: "late-usage",
      captured_through: decodeU64("0"),
      value: { input_tokens: "0" },
      stale: false,
      invalidated_through: null,
      dependency: {
        event_kinds: ["act_token_usage_finalized"] as const,
        scope: SCOPE,
        elapsed_range: null,
      },
    };
    state = reduceDiagnosticState(state, { type: "query_cached", result: lateResult });
    expect(state.queries.entries.get("late-usage")).toMatchObject({
      stale: true,
      invalidated_through: "1",
    });

    state = reduceDiagnosticState(state, {
      type: "watermark_observed",
      through_sequence: decodeU64("3"),
    });
    state = reduceDiagnosticState(state, {
      type: "query_cached",
      result: { ...lateResult, key: "missing-delivery", captured_through: decodeU64("1") },
    });
    expect(state.queries.entries.get("missing-delivery")?.invalidated_through).toBe("3");
  });

  it("preserves presentation state across incremental events", () => {
    const presentation = {
      ...createPresentationState(),
      selection: { kind: "span", id: "span:1" } as const,
      pinned_detail: { kind: "message", id: "message-1" } as const,
      expanded: ["span:1", "actor:actor-1"],
      filters: {
        event_kinds: ["span_started"] as const,
        scene_id: "scene-1",
        actor_id: "actor-1",
        text: "turn",
      },
      viewport: { start_ns: decodeU64("10"), end_ns: decodeU64("1000") },
      follow_live: false,
      zoom: { anchor_ns: decodeU64("500"), scale: 2 },
    };
    const initial = { ...createDiagnosticState(RUN_ID, decodeU64("0")), presentation };
    const next = reduceDiagnosticState(initial, {
      type: "event_received",
      event: event(1, {
        kind: "counter_sampled",
        counter_kind: "actor.mailbox_depth",
        value: "2",
      }),
    });

    expect(next.presentation).toBe(presentation);
  });

  it("freezes the presented live projection while the bounded hot edge advances", () => {
    let state = ingest(createDiagnosticState(RUN_ID, decodeU64("0")), [
      event(1, {
        kind: "agent_message_delta",
        message_id: "message-1",
        source_message_id: null,
        text_delta: "before",
      }),
    ]);
    state = reduceDiagnosticState(state, { type: "pause" });
    state = reduceDiagnosticState(state, {
      type: "event_received",
      event: event(2, {
        kind: "agent_message_delta",
        message_id: "message-1",
        source_message_id: null,
        text_delta: " after",
      }),
    });

    expect(state.live.projection.messages.items[0]?.text).toBe("before after");
    expect(state.live.observed_elapsed_ns).toBe("20");
    expect(presentedLiveEdge(state).projection.messages.items[0]?.text).toBe("before");
    expect(presentedLiveEdge(state).observed_elapsed_ns).toBe("10");
    expect(presentedLiveEdge(reduceDiagnosticState(state, { type: "resume" }))).toBe(state.live);
  });

  it("does not claim a post-snapshot message tail contains the beginning", () => {
    const state = reduceDiagnosticState(createDiagnosticState(RUN_ID, decodeU64("10")), {
      type: "event_received",
      event: event(11, {
        kind: "agent_message_delta",
        message_id: "possibly-open-at-snapshot",
        source_message_id: null,
        text_delta: "tail",
      }),
    });

    expect(state.live.projection.messages.items[0]?.text_complete_from_start).toBe(false);
    expect(state.live.projection.messages.needs_server_refresh).toBe(true);
  });
});
