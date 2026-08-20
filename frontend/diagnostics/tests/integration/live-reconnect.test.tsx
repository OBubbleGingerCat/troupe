import { beforeEach, describe, expect, it, vi } from "vitest";

import { decodeU64 } from "../../src/protocol/decimal.ts";
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

function status(watermark: string, terminal = false): Record<string, unknown> {
  return {
    api_schema_version: 1,
    run_id: RUN_ID,
    source: terminal ? "archive" : "active",
    store_schema_version: "1",
    store_schema_identity: "troupe.diagnostics.store.v1",
    event_schema_version: "1",
    configuration_identity: "configuration-sha256:w05",
    event_watermark: watermark,
    read_model_watermark: watermark,
    lifecycle: {
      state: terminal ? "completed" : "active",
      started_at: "2026-08-16T00:00:00Z",
      ended_at: terminal ? "2026-08-16T00:00:01Z" : null,
      outcome: terminal ? "completed" : null,
      clean_shutdown: terminal,
    },
    writer: { status: "unavailable", reason: terminal ? "archive" : "state_unavailable" },
    quota: { status: "unavailable", reason: terminal ? "archive" : "state_unavailable" },
  };
}

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

function eventAt(sequence: number): Record<string, unknown> {
  const fixture = loadEventFixture("custom-instant-occurred.json") as readonly Record<string, unknown>[];
  return {
    ...fixture[0],
    sequence: String(sequence),
    elapsed_ns: String(sequence * 10),
    caused_by: [],
  };
}

function eventsThrough(through: string): Record<string, unknown> {
  const count = Number(BigInt(through));
  return {
    api_schema_version: 1,
    run_id: RUN_ID,
    captured_watermark: through,
    events: Array.from({ length: count }, (_, index) => eventAt(index + 1)),
    next_after: null,
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

  open(): void {
    this.dispatch("open", new Event("open"));
  }

  error(): void {
    this.dispatch("error", new Event("error"));
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

function routedFetch(): { readonly fetch: DiagnosticFetch; readonly urls: string[] } {
  const urls: string[] = [];
  let statusCount = 0;
  let snapshotCount = 0;
  const fetch = vi.fn(async (input: RequestInfo | URL): Promise<Response> => {
    const url = new URL(input.toString());
    urls.push(url.href);
    let body: unknown;
    if (url.pathname.endsWith("/identity")) {
      body = identity();
    } else if (url.pathname.endsWith("/status")) {
      body = status(statusCount++ < 2 ? (snapshotCount === 0 ? "2" : "5") : "5", statusCount > 2);
    } else if (url.pathname.endsWith("/snapshot")) {
      body = snapshotAt(snapshotCount++ === 0 ? "2" : "5");
    } else if (url.pathname.endsWith("/events")) {
      const through = url.searchParams.get("through");
      if (url.searchParams.get("after") !== "0" || through === null) {
        throw new Error(`unexpected finite suffix request ${url.href}`);
      }
      body = eventsThrough(through);
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

function ready(source: FakeEventSource, after: string, through: string): void {
  source.emit("stream_ready", {
    control_schema_version: 1,
    run_id: RUN_ID,
    resume_after: after,
    replay_through: through,
  }, "stale-control-id");
}

describe("live diagnostics reconnect", () => {
  beforeEach(() => {
    FakeEventSource.instances.length = 0;
  });

  it("dedupes replay, resyncs gaps from server facts, preserves presentation, and closes terminally", async () => {
    const requests = routedFetch();
    const scheduleDraw = vi.fn();
    const controller = await startLiveDiagnostics({
      baseUrl: BASE_URL,
      fetch: requests.fetch,
      EventSource: FakeEventSource as EventSourceConstructor,
      scheduleDraw,
    });
    const first = FakeEventSource.instances[0];
    expect(first).toBeDefined();
    first?.open();
    ready(first!, "2", "2");

    first?.emit("diagnostic_event", eventAt(3), "3");
    const drawsAfterThree = scheduleDraw.mock.calls.length;
    first?.emit("diagnostic_event", eventAt(3), "3");
    expect(scheduleDraw).toHaveBeenCalledTimes(drawsAfterThree);
    expect(controller.state.diagnostics?.cursor.delivered_through).toBe("3");

    first?.emit("heartbeat", {
      control_schema_version: 1,
      run_id: RUN_ID,
      committed_watermark: "5",
    }, "999");
    expect(controller.state.diagnostics?.cursor).toEqual({
      delivered_through: "3",
      committed_watermark: "5",
    });

    first?.error();
    expect(controller.state.phase).toBe("reconnecting");
    expect(FakeEventSource.instances).toHaveLength(1);
    first?.open();
    ready(first!, "3", "5");
    first?.emit("diagnostic_event", eventAt(4), "4");
    first?.emit("diagnostic_event", eventAt(5), "5");

    controller.dispatch({ type: "select", selection: { kind: "event", id: "3" } });
    controller.dispatch({ type: "toggle_expanded", id: "scene-1" });
    controller.dispatch({
      type: "filters_set",
      filters: {
        event_kinds: ["custom_instant_occurred"],
        scene_id: "scene-1",
        actor_id: null,
        text: "marker",
      },
    });
    controller.dispatch({
      type: "viewport_set",
      viewport: { start_ns: decodeU64("10"), end_ns: decodeU64("50") },
    });
    controller.dispatch({ type: "follow_live_set", follow_live: false });
    controller.dispatch({
      type: "zoom_set",
      zoom: { anchor_ns: decodeU64("30"), scale: 2 },
    });
    const presentation = controller.state.diagnostics?.presentation;

    first?.emit("delivery_gap", {
      control_schema_version: 1,
      run_id: RUN_ID,
      reason: "subscriber_buffer_overflow",
      last_delivered_sequence: "5",
      committed_watermark: "5",
    }, "999");
    expect(first?.closed).toBe(true);
    expect(controller.state.phase).toBe("resyncing");
    await controller.whenSettled();

    expect(FakeEventSource.instances).toHaveLength(2);
    const second = FakeEventSource.instances[1];
    expect(second?.url).toBe("http://diagnostics.test/troupe/api/v1/events?after=5");
    expect(controller.state.snapshot?.watermark_sequence).toBe("5");
    expect(controller.state.diagnostics?.windows.visible?.events.map((event) => event.sequence)).toEqual([
      "1", "2", "3", "4", "5",
    ]);
    expect(controller.state.diagnostics?.live.projection.counters.items).toHaveLength(1);
    expect(controller.state.diagnostics?.presentation).toEqual(presentation);
    expect(requests.urls.every((value) => new URL(value).origin === "http://diagnostics.test")).toBe(true);
    expect(requests.urls.filter((value) => new URL(value).searchParams.has("through"))).toEqual([
      "http://diagnostics.test/troupe/api/v1/events?after=0&through=2",
      "http://diagnostics.test/troupe/api/v1/events?after=0&through=5",
    ]);

    second?.open();
    ready(second!, "5", "5");
    second?.emit("stream_closed", {
      control_schema_version: 1,
      run_id: RUN_ID,
      reason: "production_finished",
      committed_watermark: "5",
    }, "999");
    await controller.whenSettled();

    expect(second?.closed).toBe(true);
    expect(controller.state.phase).toBe("closed");
    expect(controller.state.connection).toBe("offline");
    expect(controller.state.outcome).toBe("completed");
    expect(controller.state.terminal_reason).toBe("production_finished");
    second?.error();
    expect(FakeEventSource.instances).toHaveLength(2);
  });
});
