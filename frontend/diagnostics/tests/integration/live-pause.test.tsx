import { beforeEach, describe, expect, it, vi } from "vitest";

import { decodeU64 } from "../../src/protocol/decimal.ts";
import { LIVE_EDGE_EVENT_CAPACITY } from "../../src/state/model.ts";
import { presentedLiveEdge } from "../../src/state/reducer.ts";
import type { DiagnosticFetch } from "../../src/live/bootstrap.ts";
import { startLiveDiagnostics } from "../../src/live/reconnect.ts";
import type {
  EventSourceConnection,
  EventSourceConstructor,
} from "../../src/live/sse.ts";
import {
  loadEventFixture,
  loadHttpFixture,
} from "../support/diagnostic-fixtures.ts";


const RUN_ID = "12345678-1234-4234-9234-123456789abc";
const BASE_URL = "http://diagnostics.test/troupe/";
const CUSTOM_EVENTS = loadEventFixture("custom-instant-occurred.json") as readonly Record<string, unknown>[];

function identity(): Record<string, unknown> {
  return {
    identity_schema_version: 1,
    server_protocol_version: 1,
    event_schema_version: 1,
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
  };
}

function activeStatus(): Record<string, unknown> {
  return {
    api_schema_version: 1,
    run_id: RUN_ID,
    source: "active",
    store_schema_version: "1",
    store_schema_identity: "troupe.diagnostics.store.v1",
    event_schema_version: "1",
    configuration_identity: "configuration-sha256:w05",
    event_watermark: "2",
    read_model_watermark: "2",
    lifecycle: {
      state: "active",
      started_at: "2026-08-16T00:00:00Z",
      ended_at: null,
      outcome: null,
      clean_shutdown: false,
    },
    writer: { status: "unavailable", reason: "state_unavailable" },
    quota: { status: "unavailable", reason: "state_unavailable" },
  };
}

function eventAt(sequence: number): Record<string, unknown> {
  const base = CUSTOM_EVENTS[0];
  if (base === undefined) {
    throw new Error("custom instant fixture is empty");
  }
  return {
    ...base,
    sequence: String(sequence),
    elapsed_ns: String(sequence * 10),
    caused_by: [],
  };
}

class FakeEventSource implements EventSourceConnection {
  static readonly instances: FakeEventSource[] = [];
  readonly readyState = 1;
  readonly url: string;
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

  close(): void {}

  emit(type: string, data: unknown, lastEventId = ""): void {
    const event = new MessageEvent(type, {
      data: JSON.stringify(data),
      lastEventId,
    });
    for (const listener of this.listeners.get(type) ?? []) {
      if (typeof listener === "function") {
        listener(event);
      } else {
        listener.handleEvent(event);
      }
    }
  }
}

function routedFetch(): { readonly fetch: DiagnosticFetch; readonly urls: string[] } {
  const urls: string[] = [];
  const fetch = vi.fn(async (input: RequestInfo | URL): Promise<Response> => {
    const url = new URL(input.toString());
    urls.push(url.href);
    const body = url.pathname.endsWith("/identity")
      ? identity()
      : url.pathname.endsWith("/status")
        ? activeStatus()
        : url.pathname.endsWith("/snapshot")
          ? loadHttpFixture("snapshot-v1.json")
          : url.pathname.endsWith("/events")
            ? loadHttpFixture("events-v1.json")
            : null;
    if (body === null) {
      throw new Error(`unexpected request ${url.href}`);
    }
    return new Response(JSON.stringify(body), {
      status: 200,
      headers: { "content-type": "application/json; charset=utf-8" },
    });
  }) as unknown as DiagnosticFetch;
  return { fetch, urls };
}

describe("live diagnostics pause", () => {
  beforeEach(() => {
    FakeEventSource.instances.length = 0;
  });

  it("keeps ingesting while presentation is frozen and emits a bounded server range intent", async () => {
    const requests = routedFetch();
    const scheduleDraw = vi.fn();
    const controller = await startLiveDiagnostics({
      baseUrl: BASE_URL,
      fetch: requests.fetch,
      EventSource: FakeEventSource as EventSourceConstructor,
      scheduleDraw,
    });
    const source = FakeEventSource.instances[0];
    expect(source).toBeDefined();
    source?.emit("stream_ready", {
      control_schema_version: 1,
      run_id: RUN_ID,
      resume_after: "2",
      replay_through: "2",
    });

    controller.dispatch({ type: "select", selection: { kind: "event", id: "2" } });
    controller.dispatch({ type: "toggle_expanded", id: "actor-1/cue-1" });
    controller.dispatch({ type: "follow_live_set", follow_live: false });
    controller.dispatch({
      type: "zoom_set",
      zoom: { anchor_ns: decodeU64("20"), scale: 1.5 },
    });
    controller.pause();
    const frozen = presentedLiveEdge(controller.state.diagnostics!);
    const drawsBeforeEvents = scheduleDraw.mock.calls.length;

    const finalSequence = LIVE_EDGE_EVENT_CAPACITY + 4;
    const receivedEvents = LIVE_EDGE_EVENT_CAPACITY + 2;
    for (let sequence = 3; sequence <= finalSequence; sequence += 1) {
      source?.emit("diagnostic_event", eventAt(sequence), String(sequence));
    }
    expect(scheduleDraw).toHaveBeenCalledTimes(drawsBeforeEvents + receivedEvents);
    source?.emit("diagnostic_event", eventAt(finalSequence), String(finalSequence));
    expect(scheduleDraw).toHaveBeenCalledTimes(drawsBeforeEvents + receivedEvents);

    const paused = controller.state.diagnostics!;
    expect(paused.cursor).toEqual({
      delivered_through: String(finalSequence),
      committed_watermark: String(finalSequence),
    });
    expect(paused.pause.unseen_count).toBe(BigInt(receivedEvents));
    expect(presentedLiveEdge(paused)).toBe(frozen);
    expect(paused.live.events).toHaveLength(LIVE_EDGE_EVENT_CAPACITY);
    expect(paused.live.dropped_through).toBe("4");
    expect(paused.presentation).toMatchObject({
      selection: { kind: "event", id: "2" },
      expanded: ["actor-1/cue-1"],
      follow_live: false,
      zoom: { anchor_ns: "20", scale: 1.5 },
    });

    const queryIntent = controller.resume();
    expect(queryIntent).toEqual({
      kind: "server_range",
      after_sequence: "2",
      through_sequence: String(finalSequence),
    });
    expect(controller.state.diagnostics?.pause).toMatchObject({
      paused: false,
      unseen_count: 0n,
      resume_request: queryIntent,
    });
    expect(requests.urls).toHaveLength(4);
    expect(requests.urls[3]).toBe(
      "http://diagnostics.test/troupe/api/v1/events?after=0&through=2",
    );
    expect(requests.urls.every((value) => new URL(value).origin === "http://diagnostics.test")).toBe(true);

    controller.consumeResumeQueryIntent();
    expect(controller.state.diagnostics?.pause.resume_request).toBeNull();
  });
});
