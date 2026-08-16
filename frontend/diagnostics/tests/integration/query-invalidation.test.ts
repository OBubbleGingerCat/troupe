import { afterEach, describe, expect, it, vi } from "vitest";

import {
  decodeCanonicalUuid,
  decodeU64,
} from "../../src/protocol/decimal.ts";
import { decodeDiagnosticScope } from "../../src/protocol/event.ts";
import { decodeViewRecord } from "../../src/protocol/view.ts";
import type { DiagnosticFetch } from "../../src/live/bootstrap.ts";
import {
  type ViewQueryContext,
  freezeViewQueryGeneration,
} from "../../src/query/binding.ts";
import { ViewQueryClient } from "../../src/query/client.ts";
import { loadHttpFixture } from "../support/diagnostic-fixtures.ts";


const RUN_ID = decodeCanonicalUuid("12345678-1234-4234-9234-123456789abc");
const OTHER_RUN_ID = decodeCanonicalUuid("aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa");
const BASE_URL = "http://diagnostics.test/troupe/";
const API_BASE_URL = `${BASE_URL}api/v1/`;

const TIME_SERIES_RECORD_WIRE = {
  renderer: "time_series",
  view_schema_version: 1,
  id: "live_series",
  title: "Live series",
  time_range: "viewport",
  scope: "selection",
  query: {
    source: {
      source: "instant_count",
      selector: { selector: "built_in", kind: "cue.admitted" },
    },
    filters: [],
    group_by: null,
    reducer: "count",
  },
} as const;

const SELECTED_SCOPE = decodeDiagnosticScope({
  scene_id: "scene-a",
  actor_id: "actor-a",
  cue_id: "cue-a",
  effect_id: null,
  act_id: "act-a",
  tool_call_id: null,
  session_generation: "1",
});

interface Deferred<T> {
  readonly promise: Promise<T>;
  readonly resolve: (value: T) => void;
}

function deferred<T>(): Deferred<T> {
  let resolve!: (value: T) => void;
  const promise = new Promise<T>((done) => {
    resolve = done;
  });
  return { promise, resolve };
}

function jsonResponse(value: unknown, status = 200): Response {
  return new Response(JSON.stringify(value), {
    status,
    headers: { "content-type": "application/json; charset=utf-8" },
  });
}

function client(fetch: DiagnosticFetch, timeout = 30_000): ViewQueryClient {
  return new ViewQueryClient({
    bootstrap: {
      origin: "http://diagnostics.test",
      api_base_url: API_BASE_URL,
      identity: { run_id: RUN_ID },
    },
    fetch,
    request_timeout_ms: timeout,
  });
}

function catalogWith(views: readonly unknown[]): unknown {
  const base = structuredClone(loadHttpFixture("view-catalog-v1.json")) as Record<string, unknown>;
  base.views = views;
  return base;
}

function runContext(): ViewQueryContext {
  return {
    captured_watermark: decodeU64("2"),
    captured_elapsed_end_ns: decodeU64("4"),
    selection: null,
    selected_scope: null,
    viewport: null,
  };
}

function seriesContext(
  start: string,
  end: string,
  watermark = "2",
  selectionId = "1",
): ViewQueryContext {
  return {
    captured_watermark: decodeU64(watermark),
    captured_elapsed_end_ns: decodeU64("4"),
    selection: { kind: "event", id: selectionId },
    selected_scope: SELECTED_SCOPE,
    viewport: { start_ns: decodeU64(start), end_ns: decodeU64(end) },
  };
}

function timeSeriesResponse(
  start: string,
  end: string,
  watermark: string,
  selectedScope: unknown = SELECTED_SCOPE,
): unknown {
  const wire = structuredClone(loadHttpFixture("view-timeseries-v1.json")) as Record<string, unknown>;
  wire.view_id = "live_series";
  const binding = wire.binding as Record<string, unknown>;
  binding.captured_watermark = watermark;
  binding.captured_elapsed_end_ns = "4";
  binding.time_range = "viewport";
  binding.range_start_ns = start;
  binding.range_end_ns = end;
  binding.scope = "selection";
  binding.selected_scope = selectedScope;
  const width = BigInt(end) === BigInt(start)
    ? 1n
    : (BigInt(end) - BigInt(start) + 1022n) / 1023n;
  wire.bucket_width_ns = (width > 1n ? width : 1n).toString();
  const series = wire.series as { points: Record<string, unknown>[] }[];
  series[0]!.points = series[0]!.points.filter((point) => {
    const pointStart = BigInt(String(point.bucket_start_ns));
    return pointStart >= BigInt(start) && pointStart < BigInt(end);
  });
  return wire;
}

afterEach(() => {
  vi.useRealTimers();
  vi.restoreAllMocks();
});

describe("view catalog lifecycle", () => {
  it("loads one atomic catalog and never queries an incompatible entry", async () => {
    const archiveCatalog = loadHttpFixture("view-catalog-archive-v1.json");
    const timeline = loadHttpFixture("view-timeline-v1.json");
    const urls: URL[] = [];
    const fetch = vi.fn(async (input: RequestInfo | URL): Promise<Response> => {
      const url = new URL(input.toString());
      urls.push(url);
      return jsonResponse(url.search === "" ? archiveCatalog : timeline);
    }) as unknown as DiagnosticFetch;
    const surface = client(fetch);

    const first = surface.loadCatalog();
    const second = surface.loadCatalog();
    expect(first).toBe(second);
    const catalog = await first;

    expect(catalog.views.map((entry) => "status" in entry ? entry.view_id : entry.id)).toEqual([
      "timeline_view",
      "future_view",
    ]);
    expect(surface.catalogState).toMatchObject({ status: "ready", catalog });
    expect(urls).toHaveLength(1);
    expect(urls[0]?.search).toBe("");

    await expect(surface.query("future_view", runContext())).rejects.toMatchObject({
      code: "incompatible",
    });
    expect(urls).toHaveLength(1);

    const result = await surface.query("timeline_view", runContext(), { page_size: 1 });
    expect(result.response.renderer).toBe("timeline");
    expect(urls).toHaveLength(2);
    expect(urls[1]?.searchParams.get("view_id")).toBe("timeline_view");
  });

  it("rejects a malformed mixed catalog without retaining a partial prefix", async () => {
    const archive = structuredClone(
      loadHttpFixture("view-catalog-archive-v1.json"),
    ) as { views: Record<string, unknown>[] };
    archive.views[1] = { ...archive.views[1]!, title: "must not be present" };
    let requests = 0;
    const fetch = vi.fn(async (): Promise<Response> => {
      requests += 1;
      return jsonResponse(archive);
    }) as unknown as DiagnosticFetch;
    const surface = client(fetch);

    await expect(surface.loadCatalog()).rejects.toMatchObject({ code: "catalog" });
    expect(surface.catalogState).toMatchObject({ status: "local_error", catalog: null });
    await expect(surface.loadCatalog()).rejects.toMatchObject({ code: "catalog" });
    await expect(surface.query("timeline_view", runContext())).rejects.toMatchObject({
      code: "catalog",
    });
    expect(requests).toBe(1);
  });
});

describe("query generation invalidation", () => {
  it("freezes every binding identity and derives TimeSeries width from the range", () => {
    const record = decodeViewRecord(TIME_SERIES_RECORD_WIRE);
    const base = {
      captured_watermark: decodeU64("7"),
      captured_elapsed_end_ns: decodeU64("2046"),
      selection: { kind: "event", id: "7" } as const,
      selected_scope: SELECTED_SCOPE,
      viewport: { start_ns: decodeU64("0"), end_ns: decodeU64("1023") },
    } satisfies ViewQueryContext;
    const generation = freezeViewQueryGeneration(RUN_ID, record, base);
    const variants = [
      freezeViewQueryGeneration(OTHER_RUN_ID, record, base),
      freezeViewQueryGeneration(RUN_ID, record, {
        ...base,
        selection: { kind: "event", id: "8" },
      }),
      freezeViewQueryGeneration(RUN_ID, record, {
        ...base,
        selected_scope: { ...SELECTED_SCOPE, actor_id: "actor-b" },
      }),
      freezeViewQueryGeneration(RUN_ID, record, {
        ...base,
        captured_watermark: decodeU64("8"),
      }),
      freezeViewQueryGeneration(RUN_ID, record, {
        ...base,
        viewport: { start_ns: decodeU64("0"), end_ns: decodeU64("2046") },
      }),
    ];

    expect(new Set([generation.key, ...variants.map((item) => item.key)]).size).toBe(6);
    expect(generation.expected_bucket_width_ns).toBe("1");
    expect(variants[4]?.expected_bucket_width_ns).toBe("2");
  });

  it("coalesces equal work, aborts or ignores stale work, and fully replaces TimeSeries data", async () => {
    const catalog = catalogWith([TIME_SERIES_RECORD_WIRE]);
    const firstResponse = deferred<Response>();
    const firstStarted = deferred<void>();
    const queryUrls: URL[] = [];
    let firstSignal: AbortSignal | undefined;
    const fetch = vi.fn(async (
      input: RequestInfo | URL,
      init?: RequestInit,
    ): Promise<Response> => {
      const url = new URL(input.toString());
      if (url.search === "") {
        return jsonResponse(catalog);
      }
      queryUrls.push(url);
      if (url.searchParams.get("viewport_start_ns") === "0") {
        firstSignal = init?.signal ?? undefined;
        firstStarted.resolve();
        return firstResponse.promise;
      }
      if (url.searchParams.get("captured_watermark") === "4") {
        return jsonResponse(timeSeriesResponse("2", "4", "3"));
      }
      return jsonResponse(timeSeriesResponse("2", "4", "3"));
    }) as unknown as DiagnosticFetch;
    const surface = client(fetch);
    await surface.loadCatalog();

    const first = surface.query("live_series", seriesContext("0", "2"));
    await firstStarted.promise;
    const coalesced = surface.query("live_series", seriesContext("0", "2"));
    expect(queryUrls).toHaveLength(1);

    const second = await surface.query("live_series", seriesContext("2", "4", "3", "2"));
    expect(firstSignal?.aborted).toBe(true);
    expect(queryUrls).toHaveLength(2);
    expect(queryUrls[1]?.searchParams.get("scene_id")).toBe("scene-a");
    expect(queryUrls[1]?.searchParams.get("actor_id")).toBe("actor-a");
    expect(queryUrls[1]?.searchParams.has("effect_id")).toBe(false);
    expect(queryUrls[1]?.searchParams.has("tool_call_id")).toBe(false);
    expect(second.time_series?.bucket_start_ns).toEqual(["2", "3"]);
    if (second.response.renderer !== "time_series") {
      throw new Error("expected TimeSeries response");
    }
    expect(second.time_series?.series[0]?.values[0]).toBe(
      second.response.series[0]?.points[0]?.value,
    );
    expect(surface.queryState("live_series")).toMatchObject({
      status: "ready",
      result: { generation: { key: second.generation.key } },
    });

    const toggledContext = {
      ...seriesContext("2", "4", "3", "2"),
      paused: true,
      follow_live: false,
    };
    const cached = await surface.query("live_series", toggledContext);
    expect(cached).toBe(second);
    expect(queryUrls).toHaveLength(2);

    const firstAssertion = expect(first).rejects.toMatchObject({ code: "stale" });
    const coalescedAssertion = expect(coalesced).rejects.toMatchObject({ code: "stale" });
    firstResponse.resolve(jsonResponse(timeSeriesResponse("0", "2", "2")));
    await Promise.all([firstAssertion, coalescedAssertion]);
    expect(surface.queryState("live_series")).toMatchObject({
      status: "ready",
      result: { generation: { key: second.generation.key } },
    });

    await expect(
      surface.query("live_series", seriesContext("2", "4", "4", "2")),
    ).rejects.toMatchObject({ code: "protocol" });
    expect(surface.queryState("live_series")).toMatchObject({
      status: "local_error",
      result: null,
    });
  });

  it("invalidates old inflight work before publishing a new local validation error", async () => {
    const pendingResponse = deferred<Response>();
    const started = deferred<AbortSignal>();
    const fetch = vi.fn(async (
      input: RequestInfo | URL,
      init?: RequestInit,
    ): Promise<Response> => {
      const url = new URL(input.toString());
      if (url.search === "") {
        return jsonResponse(catalogWith([TIME_SERIES_RECORD_WIRE]));
      }
      if (init?.signal === null || init?.signal === undefined) {
        throw new Error("query request omitted AbortSignal");
      }
      started.resolve(init.signal);
      return pendingResponse.promise;
    }) as unknown as DiagnosticFetch;
    const surface = client(fetch);
    await surface.loadCatalog();

    const oldQuery = surface.query("live_series", seriesContext("0", "2"));
    const oldSignal = await started.promise;
    await expect(
      surface.query("live_series", seriesContext("2", "4", "3"), { page_size: 1 }),
    ).rejects.toMatchObject({ code: "pagination" });
    expect(oldSignal.aborted).toBe(true);
    expect(surface.queryState("live_series")).toMatchObject({
      status: "local_error",
      error: { code: "pagination" },
    });

    const staleAssertion = expect(oldQuery).rejects.toMatchObject({ code: "stale" });
    pendingResponse.resolve(jsonResponse(timeSeriesResponse("0", "2", "2")));
    await staleAssertion;
    expect(surface.queryState("live_series")).toMatchObject({
      status: "local_error",
      error: { code: "pagination" },
    });
  });

  it("keeps timeout and renderer failures local to their view state", async () => {
    const catalog = catalogWith([TIME_SERIES_RECORD_WIRE]);
    const fetch = vi.fn((input: RequestInfo | URL, init?: RequestInit): Promise<Response> => {
      const url = new URL(input.toString());
      if (url.search === "") {
        return Promise.resolve(jsonResponse(catalog));
      }
      return new Promise<Response>((_resolve, reject) => {
        init?.signal?.addEventListener(
          "abort",
          () => reject(new DOMException("aborted", "AbortError")),
          { once: true },
        );
      });
    }) as unknown as DiagnosticFetch;
    const surface = client(fetch, 10);
    await surface.loadCatalog();
    vi.useFakeTimers();

    const pending = surface.query("live_series", seriesContext("0", "2"));
    const assertion = expect(pending).rejects.toMatchObject({ code: "timeout" });
    await vi.advanceTimersByTimeAsync(10);
    await assertion;
    expect(surface.queryState("live_series")).toMatchObject({
      status: "local_error",
      error: { code: "timeout" },
    });
    expect(surface.catalogState.status).toBe("ready");

    surface.reportRendererFailure("other_view", new Error("renderer failed"));
    expect(surface.queryState("other_view")).toMatchObject({
      status: "local_error",
      error: { code: "renderer", message: "renderer failed" },
    });
    expect(surface.queryState("live_series")).toMatchObject({
      error: { code: "timeout" },
    });
  });
});
