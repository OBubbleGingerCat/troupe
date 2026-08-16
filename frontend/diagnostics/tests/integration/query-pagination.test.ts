import { describe, expect, it, vi } from "vitest";

import { decodeCanonicalUuid, decodeU64 } from "../../src/protocol/decimal.ts";
import { decodeDiagnosticScope } from "../../src/protocol/event.ts";
import type { DiagnosticFetch } from "../../src/live/bootstrap.ts";
import type { ViewQueryContext } from "../../src/query/binding.ts";
import { ViewQueryClient } from "../../src/query/client.ts";
import { loadHttpFixture } from "../support/diagnostic-fixtures.ts";


const RUN_ID = decodeCanonicalUuid("12345678-1234-4234-9234-123456789abc");
const API_BASE_URL = "http://diagnostics.test/troupe/api/v1/";
const OPAQUE_CURSOR = "opaque+/= cursor?";

const TABLE_RECORD_WIRE = {
  renderer: "table",
  view_schema_version: 1,
  id: "table_view",
  title: "Captured table",
  time_range: "viewport",
  scope: "selection",
  query: {
    source: { source: "event", kind: "agent_message_completed" },
    filters: [],
    columns: [{ column: "sequence" }, { column: "elapsed_ns" }],
    page_size: 1,
  },
} as const;

const SCOPE = decodeDiagnosticScope({
  scene_id: "scene-1",
  actor_id: "actor-1",
  cue_id: "cue-1",
  effect_id: null,
  act_id: "act-1",
  tool_call_id: null,
  session_generation: "9",
});

function jsonResponse(value: unknown): Response {
  return new Response(JSON.stringify(value), {
    status: 200,
    headers: { "content-type": "application/json" },
  });
}

function catalog(): unknown {
  const value = structuredClone(loadHttpFixture("view-catalog-v1.json")) as Record<string, unknown>;
  value.views = [TABLE_RECORD_WIRE];
  return value;
}

function context(): ViewQueryContext {
  return {
    captured_watermark: decodeU64("2"),
    captured_elapsed_end_ns: decodeU64("4"),
    selection: { kind: "scope", id: "selected-scope-identity" },
    selected_scope: SCOPE,
    viewport: { start_ns: decodeU64("0"), end_ns: decodeU64("4") },
  };
}

function tableResponse(sequence: string, nextCursor: string | null): unknown {
  const wire = structuredClone(loadHttpFixture("view-table-v1.json")) as Record<string, unknown>;
  const binding = wire.binding as Record<string, unknown>;
  binding.time_range = "viewport";
  binding.range_start_ns = "0";
  binding.range_end_ns = "4";
  binding.scope = "selection";
  binding.selected_scope = SCOPE;
  const pagination = wire.pagination as Record<string, unknown>;
  pagination.page_size = 1;
  pagination.next_cursor = nextCursor;
  const rows = wire.rows as { sequence: string; cells: Record<string, unknown>[] }[];
  rows[0]!.sequence = sequence;
  rows[0]!.cells[0]!.value = sequence;
  rows[0]!.cells[1]!.value = sequence;
  return wire;
}

function makeClient(fetch: DiagnosticFetch): ViewQueryClient {
  return new ViewQueryClient({
    bootstrap: {
      origin: "http://diagnostics.test",
      api_base_url: API_BASE_URL,
      identity: { run_id: RUN_ID },
    },
    fetch,
  });
}

describe("captured view pagination", () => {
  it("passes an opaque cursor while retaining the exact first-page binding", async () => {
    const queryUrls: URL[] = [];
    let page = 0;
    const fetch = vi.fn(async (input: RequestInfo | URL): Promise<Response> => {
      const url = new URL(input.toString());
      if (url.search === "") {
        return jsonResponse(catalog());
      }
      queryUrls.push(url);
      page += 1;
      return jsonResponse(page === 1
        ? tableResponse("1", OPAQUE_CURSOR)
        : tableResponse("2", null));
    }) as unknown as DiagnosticFetch;
    const surface = makeClient(fetch);

    const first = await surface.query("table_view", context(), { page_size: 1 });
    const second = await surface.nextPage(first);

    expect(second).not.toBeNull();
    expect(queryUrls).toHaveLength(2);
    expect(queryUrls[1]?.searchParams.get("cursor")).toBe(OPAQUE_CURSOR);
    for (const url of queryUrls) {
      expect(url.searchParams.get("captured_watermark")).toBe("2");
      expect(url.searchParams.get("captured_elapsed_end_ns")).toBe("4");
      expect(url.searchParams.get("viewport_start_ns")).toBe("0");
      expect(url.searchParams.get("viewport_end_ns")).toBe("4");
      expect(url.searchParams.get("scene_id")).toBe("scene-1");
      expect(url.searchParams.get("actor_id")).toBe("actor-1");
      expect(url.searchParams.get("cue_id")).toBe("cue-1");
      expect(url.searchParams.get("act_id")).toBe("act-1");
      expect(url.searchParams.get("session_generation")).toBe("9");
      expect(url.searchParams.get("page_size")).toBe("1");
      expect(url.searchParams.has("effect_id")).toBe(false);
      expect(url.searchParams.has("tool_call_id")).toBe(false);
    }
    expect(second?.generation.key).toBe(first.generation.key);
    expect(second?.response.renderer).toBe("table");
    if (second?.response.renderer !== "table") {
      throw new Error("expected table response");
    }
    expect(second.response.rows.map((row) => row.sequence)).toEqual(["2"]);
    expect(surface.queryState("table_view")).toMatchObject({
      status: "ready",
      result: { response: { rows: [{ sequence: "2" }] } },
    });
  });

  it("rejects pages above 500 before issuing a query request", async () => {
    const urls: URL[] = [];
    const fetch = vi.fn(async (input: RequestInfo | URL): Promise<Response> => {
      const url = new URL(input.toString());
      urls.push(url);
      return jsonResponse(catalog());
    }) as unknown as DiagnosticFetch;
    const surface = makeClient(fetch);
    await surface.loadCatalog();

    await expect(
      surface.query("table_view", context(), { page_size: 501 }),
    ).rejects.toMatchObject({ code: "pagination" });
    expect(urls).toHaveLength(1);
    expect(urls[0]?.search).toBe("");
  });

  it("keeps page variants in the fixed 64-result cache without merging them", async () => {
    let requests = 0;
    const fetch = vi.fn(async (input: RequestInfo | URL): Promise<Response> => {
      const url = new URL(input.toString());
      if (url.search === "") {
        return jsonResponse(catalog());
      }
      requests += 1;
      return jsonResponse(tableResponse("1", null));
    }) as unknown as DiagnosticFetch;
    const surface = makeClient(fetch);

    for (let index = 0; index < 66; index += 1) {
      const result = await surface.query("table_view", context(), {
        cursor: `page-${index}`,
        page_size: 1,
      });
      expect(result.response.renderer).toBe("table");
    }

    expect(requests).toBe(66);
    expect(surface.resultCacheSize).toBe(64);
    const state = surface.queryState("table_view");
    expect(state.status).toBe("ready");
    if (state.status !== "ready" || state.result.response.renderer !== "table") {
      throw new Error("expected ready table state");
    }
    expect(state.result.response.rows).toHaveLength(1);
  });
});
