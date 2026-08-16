import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import { decodeU64 } from "../../src/protocol/decimal.ts";
import type { DiagnosticFetch } from "../../src/live/bootstrap.ts";
import {
  startLiveDiagnostics,
  type RunOutcome,
} from "../../src/live/reconnect.ts";
import type {
  EventSourceConnection,
  EventSourceConstructor,
} from "../../src/live/sse.ts";
import {
  snapshotSuffixAfter,
} from "../../src/live/snapshot.ts";
import { loadHttpFixture } from "../support/diagnostic-fixtures.ts";


const RUN_ID = "12345678-1234-4234-9234-123456789abc";
const BASE_URL = "http://diagnostics.test/troupe/";
const EVENTS_RESPONSE = loadHttpFixture("events-v1.json") as Record<string, unknown> & {
  readonly events: readonly Record<string, unknown>[];
};

interface SnapshotModelWire {
  through_sequence: string;
  through_elapsed_ns: string;
}

interface SnapshotWire {
  watermark_sequence: string;
  state: {
    through_sequence: string;
    through_elapsed_ns: string;
    spans: SnapshotModelWire;
    messages: SnapshotModelWire;
    plans: SnapshotModelWire;
    counters: SnapshotModelWire;
    usage: SnapshotModelWire;
  };
}

function snapshotAt(sequence: string): unknown {
  const snapshot = structuredClone(loadHttpFixture("snapshot-v1.json")) as SnapshotWire;
  const elapsed = String(BigInt(sequence) * 10n);
  snapshot.watermark_sequence = sequence;
  snapshot.state.through_sequence = sequence;
  snapshot.state.through_elapsed_ns = elapsed;
  for (const model of [
    snapshot.state.spans,
    snapshot.state.messages,
    snapshot.state.plans,
    snapshot.state.counters,
    snapshot.state.usage,
  ]) {
    model.through_sequence = sequence;
    model.through_elapsed_ns = elapsed;
  }
  return snapshot;
}

function oversizedSuffix(): Record<string, unknown> {
  const template = EVENTS_RESPONSE.events[1];
  if (template === undefined) {
    throw new Error("events fixture lacks a reusable event");
  }
  return {
    ...EVENTS_RESPONSE,
    captured_watermark: "4097",
    events: Array.from({ length: 4_097 }, (_, index) => ({
      ...template,
      sequence: String(index + 1),
      elapsed_ns: String((index + 1) * 10),
      caused_by: [],
    })),
  };
}

function identity(overrides: Readonly<Record<string, unknown>> = {}): Record<string, unknown> {
  return {
    identity_schema_version: 1,
    server_protocol_version: 1,
    event_schema_version: 1,
    view_schema_version: 1,
    api_schema_version: 1,
    run_id: RUN_ID,
    owner_pid: 8123,
    process_identity: "test:boot-a:8123",
    bind_host: "0.0.0.0",
    port: 39001,
    local_endpoint: "http://127.0.0.1:39001/",
    advertise_url: BASE_URL,
    base_path: "/troupe",
    api_base_path: "/troupe/api/v1",
    identity_path: "/troupe/api/v1/identity",
    security_scope: "trusted_network",
    operational_limits: { sse_reconnect_delay_ms: "1000" },
    ...overrides,
  };
}

function status(
  state: "active" | "completed" | "failed" | "incomplete" = "active",
  outcome: "completed" | "failed" | "cancelled" | null = null,
): Record<string, unknown> {
  const terminal = state === "completed" || state === "failed";
  return {
    api_schema_version: 1,
    run_id: RUN_ID,
    source: state === "active" ? "active" : "archive",
    store_schema_version: "1",
    store_schema_identity: "troupe.diagnostics.store.v1",
    event_schema_version: "1",
    configuration_identity: "configuration-sha256:w05",
    event_watermark: "2",
    read_model_watermark: "2",
    lifecycle: {
      state,
      started_at: "2026-08-16T00:00:00Z",
      ended_at: terminal ? "2026-08-16T00:00:01Z" : null,
      outcome,
      clean_shutdown: terminal,
    },
    writer: { status: "unavailable", reason: state === "active" ? "state_unavailable" : "archive" },
    quota: { status: "unavailable", reason: state === "active" ? "state_unavailable" : "archive" },
  };
}

class FakeEventSource implements EventSourceConnection {
  static readonly instances: FakeEventSource[] = [];
  readonly readyState = 1;
  readonly url: string;
  closed = false;
  private readonly listeners = new Map<string, Set<EventListenerOrEventListenerObject>>();

  constructor(url: string | URL) {
    this.url = url.toString();
    FakeEventSource.instances.push(this);
  }

  addEventListener(type: string, listener: EventListenerOrEventListenerObject): void {
    const listeners = this.listeners.get(type) ?? new Set<EventListenerOrEventListenerObject>();
    listeners.add(listener);
    this.listeners.set(type, listeners);
  }

  removeEventListener(type: string, listener: EventListenerOrEventListenerObject): void {
    this.listeners.get(type)?.delete(listener);
  }

  close(): void {
    this.closed = true;
  }

  emit(type: string, data: unknown, lastEventId = ""): void {
    this.dispatch(type, new MessageEvent(type, {
      data: JSON.stringify(data),
      lastEventId,
    }));
  }

  private dispatch(type: string, event: Event): void {
    for (const listener of this.listeners.get(type) ?? []) {
      if (typeof listener === "function") {
        listener(event);
      } else {
        listener.handleEvent(event);
      }
    }
  }
}

function routedFetch(
  identityValue: unknown,
  statusValue: unknown,
  snapshotValue: unknown = loadHttpFixture("snapshot-v1.json"),
  suffixValue: unknown = EVENTS_RESPONSE,
): { readonly fetch: DiagnosticFetch; readonly urls: string[] } {
  const urls: string[] = [];
  const fetch = vi.fn(async (input: RequestInfo | URL): Promise<Response> => {
    const url = new URL(input.toString());
    urls.push(url.href);
    let body: unknown;
    if (url.pathname.endsWith("/identity")) {
      body = identityValue;
    } else if (url.pathname.endsWith("/status")) {
      body = statusValue;
    } else if (url.pathname.endsWith("/snapshot")) {
      body = snapshotValue;
    } else if (url.pathname.endsWith("/events")) {
      body = suffixValue;
    } else {
      throw new Error(`unexpected request ${url.href}`);
    }
    return new Response(JSON.stringify(body), {
      status: 200,
      headers: { "content-type": "application/json; charset=utf-8" },
    });
  }) as unknown as DiagnosticFetch;
  return { fetch, urls };
}

describe("live diagnostics bootstrap", () => {
  beforeEach(() => {
    FakeEventSource.instances.length = 0;
  });

  afterEach(() => {
    vi.restoreAllMocks();
  });

  it("fetches the exact bounded suffix before one hydrate and EventSource after W", async () => {
    const requests = routedFetch(identity(), status());
    const scheduleDraw = vi.fn();
    const controller = await startLiveDiagnostics({
      baseUrl: BASE_URL,
      fetch: requests.fetch,
      EventSource: FakeEventSource as EventSourceConstructor,
      scheduleDraw,
    });

    expect(requests.urls.map((value) => {
      const url = new URL(value);
      return `${url.pathname}${url.search}`;
    })).toEqual([
      "/troupe/api/v1/identity",
      "/troupe/api/v1/status",
      "/troupe/api/v1/snapshot",
      "/troupe/api/v1/events?after=0&through=2",
    ]);
    expect(requests.urls.every((value) => new URL(value).origin === "http://diagnostics.test")).toBe(true);
    expect(FakeEventSource.instances).toHaveLength(1);
    expect(FakeEventSource.instances[0]?.url).toBe(
      "http://diagnostics.test/troupe/api/v1/events?after=2",
    );
    expect(controller.state.phase).toBe("connecting");
    expect(controller.state.security).toBe("trusted_network");
    expect(controller.state.outcome).toBe("running");
    expect(controller.state.diagnostics?.cursor.delivered_through).toBe("2");
    expect(controller.state.snapshot?.watermark_sequence).toBe("2");
    expect(controller.state.snapshot?.state.counters.series).toHaveLength(1);
    expect(controller.state.diagnostics?.windows.visible?.events.map((event) => event.sequence)).toEqual([
      "1", "2",
    ]);
    expect(controller.state.diagnostics?.live.projection.counters.items).toHaveLength(1);
    expect(scheduleDraw).toHaveBeenCalledTimes(1);

    FakeEventSource.instances[0]?.emit("stream_ready", {
      control_schema_version: 1,
      run_id: RUN_ID,
      resume_after: "2",
      replay_through: "2",
    }, "999");
    expect(controller.state.phase).toBe("live");
    expect(controller.state.connection).toBe("connected");
    expect(controller.state.diagnostics?.cursor.delivered_through).toBe("2");
  });

  it("computes the finite lower bound without narrowing u64 values", () => {
    expect(snapshotSuffixAfter(decodeU64("2"))).toBe("0");
    expect(snapshotSuffixAfter(decodeU64("4096"))).toBe("0");
    expect(snapshotSuffixAfter(decodeU64("4097"))).toBe("1");
    expect(snapshotSuffixAfter(decodeU64("18446744073709551615"))).toBe(
      "18446744073709547519",
    );
  });

  it("accepts a suffix captured after the snapshot while hydrating only through W", async () => {
    const suffix = { ...EVENTS_RESPONSE, captured_watermark: "3" };
    const requests = routedFetch(
      identity(),
      status(),
      loadHttpFixture("snapshot-v1.json"),
      suffix,
    );
    const controller = await startLiveDiagnostics({
      baseUrl: BASE_URL,
      fetch: requests.fetch,
      EventSource: FakeEventSource as EventSourceConstructor,
    });

    expect(controller.state.phase).toBe("connecting");
    expect(controller.state.diagnostics?.cursor).toEqual({
      delivered_through: "2",
      committed_watermark: "2",
    });
    expect(controller.state.diagnostics?.windows.visible?.events.map((event) => event.sequence))
      .toEqual(["1", "2"]);
    expect(FakeEventSource.instances[0]?.url).toBe(
      "http://diagnostics.test/troupe/api/v1/events?after=2",
    );
  });

  it.each([
    [
      "another Run",
      {
        ...EVENTS_RESPONSE,
        run_id: "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa",
        events: EVENTS_RESPONSE.events.map((event) => ({
          ...event,
          run_id: "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa",
        })),
      },
      "identity",
    ],
    [
      "a continuation cursor",
      { ...EVENTS_RESPONSE, next_after: "2" },
      "protocol",
    ],
    [
      "a non-dense range",
      {
        ...EVENTS_RESPONSE,
        events: EVENTS_RESPONSE.events.slice(1),
      },
      "protocol",
    ],
  ] as const)("fails atomically when the suffix reports %s", async (_label, suffix, code) => {
    const requests = routedFetch(identity(), status(), loadHttpFixture("snapshot-v1.json"), suffix);
    const scheduleDraw = vi.fn();
    const controller = await startLiveDiagnostics({
      baseUrl: BASE_URL,
      fetch: requests.fetch,
      EventSource: FakeEventSource as EventSourceConstructor,
      scheduleDraw,
    });

    expect(controller.state.phase).toBe("failed");
    expect(controller.state.error?.code).toBe(code);
    expect(controller.state.diagnostics).toBeNull();
    expect(FakeEventSource.instances).toHaveLength(0);
    expect(scheduleDraw).not.toHaveBeenCalled();
  });

  it("rejects more than 4096 suffix events before hydrate", async () => {
    const requests = routedFetch(identity(), status(), snapshotAt("4097"), oversizedSuffix());
    const scheduleDraw = vi.fn();
    const controller = await startLiveDiagnostics({
      baseUrl: BASE_URL,
      fetch: requests.fetch,
      EventSource: FakeEventSource as EventSourceConstructor,
      scheduleDraw,
    });

    expect(requests.urls[requests.urls.length - 1]).toBe(
      "http://diagnostics.test/troupe/api/v1/events?after=1&through=4097",
    );
    expect(controller.state).toMatchObject({
      phase: "failed",
      diagnostics: null,
      error: { code: "protocol" },
    });
    expect(FakeEventSource.instances).toHaveLength(0);
    expect(scheduleDraw).not.toHaveBeenCalled();
  });

  it("stays static when a declared schema is incompatible", async () => {
    const requests = routedFetch(identity({ view_schema_version: 2 }), status());
    const controller = await startLiveDiagnostics({
      baseUrl: BASE_URL,
      fetch: requests.fetch,
      EventSource: FakeEventSource as EventSourceConstructor,
    });

    expect(controller.state.phase).toBe("compatibility");
    expect(controller.state.bootstrap?.compatibility.decisions.view.status).toBe("incompatible");
    expect(requests.urls.some((value) => new URL(value).pathname.endsWith("/snapshot"))).toBe(false);
    expect(FakeEventSource.instances).toHaveLength(0);
  });

  it.each([
    ["failed", "failed", "failed"],
    ["failed", "cancelled", "cancelled"],
    ["incomplete", null, "incomplete"],
  ] as const)(
    "reports archive lifecycle %s/%s without opening live transport",
    async (state, terminalOutcome, expectedOutcome) => {
      const requests = routedFetch(identity(), status(state, terminalOutcome));
      const controller = await startLiveDiagnostics({
        baseUrl: BASE_URL,
        fetch: requests.fetch,
        EventSource: FakeEventSource as EventSourceConstructor,
      });

      expect(controller.state.phase).toBe("archive");
      expect(controller.state.connection).toBe("archive");
      expect(controller.state.outcome).toBe(expectedOutcome satisfies RunOutcome);
      expect(FakeEventSource.instances).toHaveLength(0);
    },
  );
});
