import { describe, expect, it } from "vitest";

import type {
  DiagnosticBootstrap,
  DiagnosticFetch,
} from "../../src/live/bootstrap.ts";
import { decodeU64 } from "../../src/protocol/decimal.ts";
import { fetchTimelineHistoryCapture } from "../../src/timeline/history_capture.ts";


const RUN_ID = "12345678-1234-4234-9234-123456789abc";
const BOOTSTRAP = {
  origin: "http://diagnostics.test",
  api_base_url: "http://diagnostics.test/api/v1/",
  identity: { run_id: RUN_ID },
} as unknown as Pick<DiagnosticBootstrap, "origin" | "api_base_url" | "identity">;

function rawEvent(sequence: number): Readonly<Record<string, unknown>> {
  return {
    schema_version: 1,
    run_id: RUN_ID,
    sequence: String(sequence),
    elapsed_ns: String(sequence * 1_000_000_000),
    scope: {
      scene_id: null,
      actor_id: null,
      cue_id: null,
      effect_id: null,
      act_id: null,
      tool_call_id: null,
      session_generation: null,
    },
    caused_by: [],
    kind: "instant_occurred",
    instant_kind: "cue.admitted",
    detail: {},
    containing_span_id: null,
  };
}

function response(events: readonly Readonly<Record<string, unknown>>[]): Response {
  return new Response(JSON.stringify({
    api_schema_version: 1,
    run_id: RUN_ID,
    captured_watermark: "3",
    events,
    next_after: null,
  }), {
    status: 200,
    headers: { "content-type": "application/json" },
  });
}

describe("Timeline History capture", () => {
  it("freezes and validates the exact event prefix at the requested watermark", async () => {
    const requests: string[] = [];
    const fetchImpl: DiagnosticFetch = async (input) => {
      requests.push(String(input));
      return response([rawEvent(1), rawEvent(2)]);
    };

    const capture = await fetchTimelineHistoryCapture(
      BOOTSTRAP,
      decodeU64("2"),
      fetchImpl,
    );

    expect(requests).toEqual(["http://diagnostics.test/api/v1/events?after=0&through=2"]);
    expect(capture.through).toBe("2");
    expect(capture.response.events.map((event) => event.sequence)).toEqual(["1", "2"]);
  });

  it("rejects a capture that omits an earlier event", async () => {
    const fetchImpl: DiagnosticFetch = async () => response([rawEvent(2)]);

    await expect(fetchTimelineHistoryCapture(
      BOOTSTRAP,
      decodeU64("2"),
      fetchImpl,
    )).rejects.toThrow(/exact dense range/);
  });
});
