import { describe, expect, it } from "vitest";

import {
  ProtocolDecodeError,
  decodeTokenInteger,
  decodeU64,
} from "../../src/protocol/decimal.ts";
import {
  DIAGNOSTIC_EVENT_KINDS,
  decodeDiagnosticEvent,
  decodeDiagnosticEventJson,
  encodeDiagnosticEventJson,
} from "../../src/protocol/event.ts";
import {
  loadAllValidEventFixtures,
  loadMalformedEventCases,
} from "../support/diagnostic-fixtures.ts";


describe("canonical diagnostic event protocol", () => {
  it("decodes every shared fixture as the closed fourteen-event union", () => {
    const rawEvents = loadAllValidEventFixtures();
    const decoded = rawEvents.map((event) => decodeDiagnosticEvent(event));
    const observedKinds = new Set(decoded.map((event) => event.kind));

    expect(observedKinds).toEqual(new Set(DIAGNOSTIC_EVENT_KINDS));
    expect(DIAGNOSTIC_EVENT_KINDS).toHaveLength(14);
    for (let index = 0; index < decoded.length; index += 1) {
      expect(JSON.parse(encodeDiagnosticEventJson(decoded[index]!))).toEqual(
        rawEvents[index],
      );
      expect(decodeDiagnosticEventJson(encodeDiagnosticEventJson(decoded[index]!))).toEqual(
        decoded[index],
      );
      expect(typeof decoded[index]!.sequence).toBe("string");
      expect(typeof decoded[index]!.elapsed_ns).toBe("string");
    }
  });

  it("rejects the shared malformed matrix with stable protocol errors", () => {
    for (const testCase of loadMalformedEventCases()) {
      expect(
        () => decodeDiagnosticEvent(testCase.event),
        testCase.name,
      ).toThrow(ProtocolDecodeError);
      try {
        decodeDiagnosticEvent(testCase.event);
      } catch (error) {
        expect(error).toBeInstanceOf(ProtocolDecodeError);
        expect((error as ProtocolDecodeError).code).toBe(testCase.expected_error);
      }
    }
  });

  it("keeps arbitrary token integers exact without widening u64 identity", () => {
    const arbitrary = "12345678901234567890123456789012345678901234567890";

    expect(decodeTokenInteger(arbitrary)).toBe(arbitrary);
    expect(() => decodeU64(arbitrary)).toThrowError(/u64/);
    expect(decodeU64("18446744073709551615")).toBe("18446744073709551615");
    expect(() => decodeU64("18446744073709551616")).toThrowError(/u64/);
  });

  it("preserves user text as plain data instead of interpreting or rewriting it", () => {
    const source = loadAllValidEventFixtures().find(
      (event) => (event as { kind?: unknown }).kind === "agent_message_delta",
    ) as Record<string, unknown>;
    const text = '<script>alert("diagnostic")</script> **not markdown**';
    const event = decodeDiagnosticEvent({ ...source, text_delta: text });

    expect(event.kind).toBe("agent_message_delta");
    if (event.kind === "agent_message_delta") {
      expect(event.text_delta).toBe(text);
    }
    expect(encodeDiagnosticEventJson(event)).toContain("<script>");
  });

  it("requires explicit optional nulls and rejects unowned fields", () => {
    const source = loadAllValidEventFixtures().find(
      (event) => (event as { kind?: unknown }).kind === "span_finished",
    ) as Record<string, unknown>;
    const { error_code: _removed, ...missingOptional } = source;

    expect(() => decodeDiagnosticEvent(missingOptional)).toThrowError(/fields/);
    expect(() => decodeDiagnosticEvent({ ...source, raw_payload: { secret: true } })).toThrowError(
      /fields/,
    );
  });
});
