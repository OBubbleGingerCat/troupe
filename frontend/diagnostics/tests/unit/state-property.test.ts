import { readFileSync } from "node:fs";
import { resolve } from "node:path";

import { describe, expect, it } from "vitest";

import { decodeCanonicalUuid, decodeU64 } from "../../src/protocol/decimal.ts";
import { decodeDiagnosticEvent } from "../../src/protocol/event.ts";
import {
  ACT_USAGE_CAPACITY,
  CONTEXT_USAGE_CAPACITY,
  COUNTER_SERIES_CAPACITY,
  EXPANDED_ITEM_CAPACITY,
  GAP_CAPACITY,
  LIVE_EDGE_EVENT_CAPACITY,
  MESSAGE_CAPACITY,
  QUERY_RESULT_CAPACITY,
  RESULT_FACT_CAPACITY,
  SPAN_CAPACITY,
  TOOL_FACT_CAPACITY,
} from "../../src/state/model.ts";
import { createDiagnosticState, reduceDiagnosticState } from "../../src/state/reducer.ts";


const RUN_ID = decodeCanonicalUuid("12345678-1234-4234-9234-123456789abc");

function generatedEvent(sequence: number) {
  const suffix = String(sequence);
  const common = {
    schema_version: 1,
    run_id: RUN_ID,
    sequence: suffix,
    elapsed_ns: suffix,
    scope: {
      scene_id: `scene-${sequence % 7}`,
      actor_id: `actor-${sequence % 13}`,
      cue_id: `cue-${sequence}`,
      effect_id: null,
      act_id: `act-${sequence}`,
      tool_call_id: null,
      session_generation: "1",
    },
    caused_by: [],
  };
  switch (sequence % 8) {
    case 0:
      return decodeDiagnosticEvent({
        ...common,
        kind: "span_started",
        span_kind: "cue.execution",
        detail: {},
        parent_span_id: null,
      });
    case 1:
      return decodeDiagnosticEvent({
        ...common,
        kind: "agent_message_delta",
        message_id: `message-${sequence}`,
        source_message_id: null,
        text_delta: suffix,
      });
    case 2:
      return decodeDiagnosticEvent({
        ...common,
        kind: "custom_counter_sampled",
        name: "example.queue_depth",
        value: { type: "integer", value: suffix },
        unit: null,
        dimensions: { shard: { type: "string", value: suffix } },
      });
    case 3:
      return decodeDiagnosticEvent({
        ...common,
        kind: "context_usage_sampled",
        context_used_tokens: suffix,
        context_window_tokens: "100000",
        cumulative_cost_amount: null,
        cumulative_cost_currency: null,
        sample_origin: "provider",
        observed_elapsed_ns: null,
      });
    case 4:
      return decodeDiagnosticEvent({
        ...common,
        kind: "act_token_usage_finalized",
        availability: "partial",
        source: "acp.prompt_response.usage",
        unavailable_reason: null,
        provider_total_tokens: null,
        input_tokens: suffix,
        output_tokens: null,
        thought_tokens: null,
        cached_read_tokens: null,
        cached_write_tokens: null,
      });
    case 5:
      return decodeDiagnosticEvent({
        ...common,
        kind: "observation_gap",
        producer: "runtime",
        component: null,
        reason: "test-gap",
        dropped_count: "1",
        affected_elapsed: null,
        affected_kind: null,
        affected_scope: null,
      });
    case 6:
      return decodeDiagnosticEvent({
        ...common,
        scope: { ...common.scope, tool_call_id: `tool-${sequence}` },
        kind: "instant_occurred",
        instant_kind: "tool.updated",
        detail: {
          title: `Tool ${sequence}`,
          tool_kind: "read",
          status: "in_progress",
          error_code: null,
        },
        containing_span_id: null,
      });
    default:
      return decodeDiagnosticEvent({
        ...common,
        kind: "instant_occurred",
        instant_kind: "result.submitted",
        detail: { issue: null, error_code: null },
        containing_span_id: null,
      });
  }
}

describe("state invariants over long deterministic streams", () => {
  it("makes arbitrary duplicate replay a no-op and keeps a strict monotonic cursor", () => {
    let state = createDiagnosticState(RUN_ID, decodeU64("0"));
    for (let sequence = 1; sequence <= 600; sequence += 1) {
      const nextEvent = generatedEvent(sequence);
      const next = reduceDiagnosticState(state, { type: "event_received", event: nextEvent });
      expect(next.cursor.delivered_through).toBe(String(sequence));
      expect(reduceDiagnosticState(next, { type: "event_received", event: nextEvent })).toBe(next);
      state = next;
    }
  });

  it("never exceeds a released capacity and records projection loss", () => {
    const total = 3_600;
    let state = createDiagnosticState(RUN_ID, decodeU64("0"));
    for (let sequence = 1; sequence <= total; sequence += 1) {
      state = reduceDiagnosticState(state, {
        type: "event_received",
        event: generatedEvent(sequence),
      });
    }

    const projection = state.live.projection;
    expect(state.live.events.length).toBeLessThanOrEqual(LIVE_EDGE_EVENT_CAPACITY);
    expect(projection.spans.items.length).toBeLessThanOrEqual(SPAN_CAPACITY);
    expect(projection.messages.items.length).toBeLessThanOrEqual(MESSAGE_CAPACITY);
    expect(projection.counters.items.length).toBeLessThanOrEqual(COUNTER_SERIES_CAPACITY);
    expect(projection.context_usage.items.length).toBeLessThanOrEqual(CONTEXT_USAGE_CAPACITY);
    expect(projection.act_usage.items.length).toBeLessThanOrEqual(ACT_USAGE_CAPACITY);
    expect(projection.tools.items.length).toBeLessThanOrEqual(TOOL_FACT_CAPACITY);
    expect(projection.results.items.length).toBeLessThanOrEqual(RESULT_FACT_CAPACITY);
    expect(projection.gaps.items.length).toBeLessThanOrEqual(GAP_CAPACITY);
    expect(state.live.dropped_through).not.toBeNull();
    expect(projection.spans.dropped_through).not.toBeNull();
    expect(projection.messages.needs_server_refresh).toBe(true);
    expect(projection.tools.dropped_through).not.toBeNull();
    expect(projection.results.dropped_through).not.toBeNull();
  });

  it("keeps the u64 maximum exact without converting identity or time to number", () => {
    const maximum = decodeU64("18446744073709551615");
    const state = createDiagnosticState(RUN_ID, maximum, maximum);

    expect(state.cursor.delivered_through).toBe("18446744073709551615");
    expect(state.cursor.committed_watermark).toBe("18446744073709551615");
    expect(state.live.observed_elapsed_ns).toBe("18446744073709551615");
    expect(typeof state.cursor.delivered_through).toBe("string");
  });

  it("bounds expanded identities and server query values independently of selection", () => {
    let state = createDiagnosticState(RUN_ID, decodeU64("0"));
    state = reduceDiagnosticState(state, {
      type: "select",
      selection: { kind: "span", id: "selected-span" },
    });
    for (let index = 0; index < EXPANDED_ITEM_CAPACITY + 3; index += 1) {
      state = reduceDiagnosticState(state, { type: "toggle_expanded", id: `expanded-${index}` });
    }
    for (let index = 0; index < QUERY_RESULT_CAPACITY + 3; index += 1) {
      state = reduceDiagnosticState(state, {
        type: "query_cached",
        result: {
          key: `query-${index}`,
          captured_through: decodeU64("0"),
          value: { index },
          stale: false,
          invalidated_through: null,
          dependency: { event_kinds: null, scope: null, elapsed_range: null },
        },
      });
    }

    expect(state.presentation.expanded).toHaveLength(EXPANDED_ITEM_CAPACITY);
    expect(state.presentation.expanded).not.toContain("expanded-0");
    expect(state.queries.entries.size).toBe(QUERY_RESULT_CAPACITY);
    expect(state.queries.entries.has("query-0")).toBe(false);
    expect(state.presentation.selection).toEqual({ kind: "span", id: "selected-span" });
  });

  it("keeps the state layer framework-independent and memory-only", () => {
    const root = resolve(import.meta.dirname, "../../src/state");
    const source = ["model.ts", "reducer.ts", "windows.ts", "lru.ts", "queries.ts", "selection.ts"]
      .map((file) => readFileSync(resolve(root, file), "utf8"))
      .join("\n");

    for (const forbidden of [
      "indexedDB",
      "localStorage",
      "serviceWorker",
      "Worker(",
      "WebGL",
      "EventSource",
      "fetch(",
      "preact",
      "uplot",
      "document.",
      "globalThis.window",
      "canvas",
    ]) {
      expect(source).not.toContain(forbidden);
    }
  });
});
