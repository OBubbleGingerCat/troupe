import { describe, expect, it } from "vitest";

import { decodeCanonicalUuid, decodeU64 } from "../../src/protocol/decimal.ts";
import {
  type DiagnosticEvent,
  type DiagnosticScope,
  decodeDiagnosticEvent,
} from "../../src/protocol/event.ts";
import {
  createDiagnosticState,
  presentedLiveEdge,
  reduceDiagnosticState,
} from "../../src/state/reducer.ts";
import { cacheQueryResult } from "../../src/state/queries.ts";
import { createPresentationState } from "../../src/state/selection.ts";


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

describe("diagnostic state reducer", () => {
  it("projects span, message, counter, usage, and gap facts from a contiguous stream", () => {
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
        kind: "span_finished",
        span_id: "1",
        outcome: "completed",
        error_code: null,
      }),
    ];

    const state = ingest(createDiagnosticState(RUN_ID, decodeU64("0")), events);

    expect(state.cursor.delivered_through).toBe("9");
    expect(state.cursor.committed_watermark).toBe("9");
    expect(state.delivery_issue).toBeNull();
    expect(state.live.projection.spans.items[0]?.finish?.sequence).toBe("9");
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
    expect(state.live.projection.gaps.items[0]?.sequence).toBe("8");
    expect(state.live.projection.gaps.declared_dropped_count).toBe(2n);
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
    expect(presentedLiveEdge(state).projection.messages.items[0]?.text).toBe("before");
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
