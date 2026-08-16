import { describe, expect, it } from "vitest";

import {
  SUPPORTED_SCHEMA_VERSIONS,
  evaluateProtocolCompatibility,
} from "../../src/protocol/compatibility.ts";
import {
  decodeU64,
  viewportDeltaToNumber,
} from "../../src/protocol/decimal.ts";
import {
  decodeEventsResponse,
  decodeSnapshotResponse,
} from "../../src/protocol/http.ts";
import {
  SSE_CONTROL_NAMES,
  decodeSseFrame,
} from "../../src/protocol/sse.ts";
import {
  loadAllValidEventFixtures,
  loadHttpFixture,
  readProtocolSource,
} from "../support/diagnostic-fixtures.ts";


const runId = "12345678-1234-4234-9234-123456789abc";

describe("transport controls and compatibility", () => {
  it("decodes the closed no-id control union without producing a cursor", () => {
    const payloads = {
      stream_ready: {
        control_schema_version: 1,
        run_id: runId,
        resume_after: "1042",
        replay_through: "1050",
      },
      heartbeat: {
        control_schema_version: 1,
        run_id: runId,
        committed_watermark: "1050",
      },
      delivery_gap: {
        control_schema_version: 1,
        run_id: runId,
        reason: "subscriber_buffer_overflow",
        last_delivered_sequence: "1047",
        committed_watermark: "1050",
      },
      resync_required: {
        control_schema_version: 1,
        run_id: runId,
        reason: "cursor_unavailable",
        committed_watermark: "1050",
        earliest_available_sequence: "1",
      },
      stream_closed: {
        control_schema_version: 1,
        run_id: runId,
        reason: "production_finished",
        committed_watermark: "1050",
      },
    } as const;

    expect(SSE_CONTROL_NAMES).toEqual(Object.keys(payloads));
    for (const name of SSE_CONTROL_NAMES) {
      const frame = decodeSseFrame({ event: name, id: null, data: payloads[name] });
      expect(frame.frame_type).toBe("control");
      expect(frame.id).toBeNull();
      expect(frame.name).toBe(name);
    }
  });

  it("requires canonical matching IDs only for diagnostic event frames", () => {
    const raw = loadAllValidEventFixtures()[0]!;
    const sequence = (raw as { sequence: string }).sequence;
    const frame = decodeSseFrame({ event: "diagnostic_event", id: sequence, data: raw });
    expect(frame.frame_type).toBe("event");
    expect(frame.id).toBe(sequence);

    expect(() => decodeSseFrame({ event: "diagnostic_event", id: "0", data: raw })).toThrow();
    expect(() => decodeSseFrame({
      event: "heartbeat",
      id: "1050",
      data: {
        control_schema_version: 1,
        run_id: runId,
        committed_watermark: "1050",
      },
    })).toThrow();
    expect(() => decodeSseFrame({ event: "future_control", id: null, data: {} })).toThrow();
  });

  it("checks event, API, control, View and UI versions independently", () => {
    const result = evaluateProtocolCompatibility({
      event: 1,
      api: 2,
      control: 1,
      view: 1,
      ui: 1,
    });

    expect(SUPPORTED_SCHEMA_VERSIONS).toEqual({ event: 1, api: 1, control: 1, view: 1, ui: 1 });
    expect(result.mode).toBe("static");
    expect(result.decisions.api.status).toBe("incompatible");
    expect(result.decisions.event.status).toBe("compatible");
    expect(result.decisions.control.status).toBe("compatible");
    expect(result.decisions.view.status).toBe("compatible");
    expect(result.decisions.ui.status).toBe("compatible");
  });

  it("only converts a bounded viewport-relative delta to number", () => {
    const origin = decodeU64("18446744073709550000");
    const elapsed = decodeU64("18446744073709551615");
    expect(viewportDeltaToNumber(elapsed, origin, 2_000)).toBe(1_615);
    expect(() => viewportDeltaToNumber(elapsed, decodeU64("0"), 2_000)).toThrow();
  });

  it("decodes snapshot and finite events without changing cursor identity", () => {
    const rawEvent = loadAllValidEventFixtures()[0]!;
    const snapshot = decodeSnapshotResponse(loadHttpFixture("snapshot-v1.json"));
    expect(snapshot.watermark_sequence).toBe("2");
    expect(snapshot.state.usage.usages[0]?.event.provider_total_tokens).toBe(
      "1234567890123456789012345678901234567890",
    );
    expect(snapshot.state.usage.aggregate.finalized_acts).toBe("1");
    expect(snapshot.state.usage.scoped_aggregates.map((item) => (
      [item.scope.scene_id, item.scope.actor_id]
    ))).toEqual([
      ["scene-1", null],
      ["scene-1", "actor-1"],
    ]);

    const events = decodeEventsResponse({
      api_schema_version: 1,
      run_id: runId,
      captured_watermark: (rawEvent as { sequence: string }).sequence,
      events: [rawEvent],
      next_after: null,
    });
    expect(events.events[0]!.sequence).toBe((rawEvent as { sequence: string }).sequence);
  });

  it("rejects malformed or ambiguous usage snapshot facts", () => {
    const fixture = loadHttpFixture("snapshot-v1.json");
    const malformed = structuredClone(fixture) as {
      state: { usage: { scoped_aggregates: { scope: { actor_id: string | null } }[] } };
    };
    malformed.state.usage.scoped_aggregates[0]!.scope.actor_id = "actor-1";
    expect(() => decodeSnapshotResponse(malformed)).toThrow(/scoped aggregate/);

    const coverage = structuredClone(fixture) as {
      state: { usage: { aggregate: { finalized_acts: string } } };
    };
    coverage.state.usage.aggregate.finalized_acts = "2";
    expect(() => decodeSnapshotResponse(coverage)).toThrow(/availability counts/);

    const wrongSum = structuredClone(fixture) as {
      state: { usage: { aggregate: { input_tokens: { known_sum: string } } } };
    };
    wrongSum.state.usage.aggregate.input_tokens.known_sum = "41";
    expect(() => decodeSnapshotResponse(wrongSum)).toThrow(/does not match terminal usage facts/);

    const reordered = structuredClone(fixture) as {
      state: { usage: { scoped_aggregates: unknown[] } };
    };
    reordered.state.usage.scoped_aggregates.reverse();
    expect(() => decodeSnapshotResponse(reordered)).toThrow(/order or value/);
  });

  it("strictly binds every materialized snapshot envelope", () => {
    const fixture = loadHttpFixture("snapshot-v1.json");
    const unknownState = structuredClone(fixture) as { state: Record<string, unknown> };
    unknownState.state.unknown = true;
    expect(() => decodeSnapshotResponse(unknownState)).toThrow(/extra=.*unknown/);

    const wrongChildWatermark = structuredClone(fixture) as {
      state: { counters: { through_sequence: string } };
    };
    wrongChildWatermark.state.counters.through_sequence = "1";
    expect(() => decodeSnapshotResponse(wrongChildWatermark)).toThrow(/materialized model differs/);

    const wrongUsageElapsed = structuredClone(fixture) as {
      state: { usage: { through_elapsed_ns: string } };
    };
    wrongUsageElapsed.state.usage.through_elapsed_ns = "19";
    expect(() => decodeSnapshotResponse(wrongUsageElapsed)).toThrow(/usage snapshot differs/);

    const malformedTruncation = structuredClone(fixture) as {
      state: { usage: { usages: { scope: unknown }[] }; truncations: unknown[] };
    };
    malformedTruncation.state.truncations = [{
      source: "agent_message",
      sequence: "1",
      scope: malformedTruncation.state.usage.usages[0]!.scope,
      message_id: "",
    }];
    expect(() => decodeSnapshotResponse(malformedTruncation)).toThrow(/run_local_id/);

    const reorderedTruncations = structuredClone(fixture) as {
      state: { usage: { usages: { scope: unknown }[] }; truncations: unknown[] };
    };
    const truncationScope = reorderedTruncations.state.usage.usages[0]!.scope;
    reorderedTruncations.state.truncations = [
      { source: "agent_plan", sequence: "2", scope: truncationScope },
      {
        source: "agent_message",
        sequence: "1",
        scope: truncationScope,
        message_id: "message-1",
      },
    ];
    expect(() => decodeSnapshotResponse(reorderedTruncations)).toThrow(/canonical snapshot order/);
  });

  it("keeps protocol modules independent from rendering and network globals", () => {
    for (const file of ["decimal.ts", "event.ts", "view.ts", "http.ts", "sse.ts", "compatibility.ts"]) {
      const source = readProtocolSource(file);
      expect(source).not.toMatch(/from\s+["'](?:preact|@preact\/signals)/);
      expect(source).not.toMatch(/\b(?:document|window|EventSource)\s*[.(]/);
      expect(source).not.toMatch(/\bfetch\s*\(/);
    }
  });
});
