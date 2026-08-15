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
    const snapshot = decodeSnapshotResponse({
      api_schema_version: 1,
      run_id: runId,
      watermark_sequence: "0",
      earliest_available_sequence: null,
      state: {},
    });
    expect(snapshot.watermark_sequence).toBe("0");
    expect(snapshot.earliest_available_sequence).toBeNull();

    const events = decodeEventsResponse({
      api_schema_version: 1,
      run_id: runId,
      captured_watermark: (rawEvent as { sequence: string }).sequence,
      events: [rawEvent],
      next_after: null,
    });
    expect(events.events[0]!.sequence).toBe((rawEvent as { sequence: string }).sequence);
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
