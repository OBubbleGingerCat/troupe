import { spawnSync } from "node:child_process";
import { createHash } from "node:crypto";
import { readFileSync, writeFileSync } from "node:fs";
import { resolve } from "node:path";
import { inflateSync } from "node:zlib";

import { expect, test, type Page } from "@playwright/test";
import { createServer, type Plugin, type ViteDevServer } from "vite";

import {
  VISUAL_VIEWPORTS,
  type VisualScenario,
  type VisualViewportName,
} from "./viewports.ts";


const PROJECT_ROOT = resolve(import.meta.dirname, "../../..");
const REPOSITORY_ROOT = resolve(PROJECT_ROOT, "../..");
const BASELINE_ROOT = resolve(import.meta.dirname, "baselines");
const PIXEL_ORACLE = JSON.parse(
  readFileSync(resolve(import.meta.dirname, "pixel-oracle.json"), "utf8"),
) as {
  readonly channel_delta: number;
  readonly maximum_changed_pixel_ratio: number;
  readonly minimum_canvas_painted_pixels: number;
  readonly row_alignment_tolerance_css_pixels: number;
};
const SCREENSHOTS = JSON.parse(
  readFileSync(resolve(import.meta.dirname, "screenshot-manifest.json"), "utf8"),
) as {
  readonly captures: readonly {
    readonly file: string;
    readonly sha256: string;
    readonly width: number;
    readonly height: number;
    readonly capture_engine: string;
  }[];
};
const VIEW_FIXTURE = JSON.parse(
  readFileSync(resolve(REPOSITORY_ROOT, "tests/fixtures/diagnostics/views/timeseries.json"), "utf8"),
) as unknown;
const VIEW_CAPABILITIES = (JSON.parse(
  readFileSync(resolve(REPOSITORY_ROOT, "tests/fixtures/diagnostics/http/view-catalog-v1.json"), "utf8"),
) as { readonly capabilities: unknown }).capabilities;
const LOOPBACK_NO_PROXY = "127.0.0.1,localhost,::1";
const ALPHA_TEXT = "Alpha completed the first cue with a concise response.";
process.env.NO_PROXY = LOOPBACK_NO_PROXY;
process.env.no_proxy = LOOPBACK_NO_PROXY;

const FIXTURE_TEMPLATE = String.raw`
import { h, render } from "preact";
import { App } from "/src/app.tsx";
import { decodeDiagnosticEvent } from "/src/protocol/event.ts";
import { decodeViewRecord, decodeViewResponse } from "/src/protocol/view.ts";
import { freezeViewQueryGeneration } from "/src/query/binding.ts";
import { toTimeSeriesColumnarModel } from "/src/query/client.ts";
import { freezeViewPagination } from "/src/query/pagination.ts";
import { createDiagnosticState, reduceDiagnosticState } from "/src/state/reducer.ts";

const RUN_ID = "12345678-1234-4234-9234-123456789abc";
const source = new URL(location.href).searchParams.get("source") === "archive" ? "archive" : "active";
const actorScope = (scene, actor, cue = null, act = null, tool = null) => ({
  scene_id: scene, actor_id: actor, cue_id: cue, effect_id: null,
  act_id: act, tool_call_id: tool, session_generation: actor === null ? null : "1",
});
const span = (sequence, kind, scope, detail = {}) => ({
  kind: "span_started", schema_version: 1, run_id: RUN_ID, sequence: String(sequence),
  elapsed_ns: String(sequence * 10), scope, caused_by: [], span_kind: kind,
  detail, parent_span_id: null,
});
const finish = (sequence, spanId, scope, outcome = "completed") => ({
  kind: "span_finished", schema_version: 1, run_id: RUN_ID, sequence: String(sequence),
  elapsed_ns: String(sequence * 10), scope, caused_by: [], span_id: String(spanId),
  outcome, error_code: outcome === "failed" ? "fixture_failure" : null,
});
const message = (sequence, scope, id, text) => ({
  kind: "agent_message_delta", schema_version: 1, run_id: RUN_ID,
  sequence: String(sequence), elapsed_ns: String(sequence * 10), scope, caused_by: [],
  message_id: id, source_message_id: null, text_delta: text,
});
const completed = (sequence, scope, id, text) => ({
  kind: "agent_message_completed", schema_version: 1, run_id: RUN_ID,
  sequence: String(sequence), elapsed_ns: String(sequence * 10), scope, caused_by: [],
  message_id: id, utf8_bytes: String(new TextEncoder().encode(text).length),
  unicode_scalar_count: String([...text].length), truncated: false,
});
const alphaScene = actorScope("scene-alpha", null);
const alphaActor = actorScope("scene-alpha", "actor-alpha");
const alphaOne = actorScope("scene-alpha", "actor-alpha", "cue-one");
const alphaActOne = actorScope("scene-alpha", "actor-alpha", "cue-one", "act-one");
const alphaTool = actorScope("scene-alpha", "actor-alpha", "cue-one", "act-one", "tool-alpha");
const alphaTwo = actorScope("scene-alpha", "actor-alpha", "cue-two");
const alphaActTwo = actorScope("scene-alpha", "actor-alpha", "cue-two", "act-two");
const betaScene = actorScope("scene-beta", null);
const betaActor = actorScope("scene-beta", "actor-beta");
const betaCue = actorScope("scene-beta", "actor-beta", "cue-beta");
const betaAct = actorScope("scene-beta", "actor-beta", "cue-beta", "act-beta");
const alphaText = "Alpha completed the first cue with a concise response.";
const events = [
  span(1, "scene.lifecycle", alphaScene),
  span(2, "actor.handle_lifetime", alphaActor, { display_name: "Analyst Alpha", actor_type: "ResearchActor" }),
  span(3, "cue.mailbox_wait", alphaOne),
  finish(4, 3, alphaOne),
  span(5, "cue.execution", alphaOne),
  span(6, "act.lifecycle", alphaActOne, { provider: "fixture", effective_model: "model-a", effective_effort: "high" }),
  span(7, "agent.thinking", alphaActOne),
  finish(8, 7, alphaActOne),
  span(9, "tool.call", alphaTool, { title: "Search records", tool_kind: "search", status: "in_progress", error_code: null }),
  {
    kind: "instant_occurred", schema_version: 1, run_id: RUN_ID, sequence: "10",
    elapsed_ns: "100", scope: alphaTool, caused_by: [], instant_kind: "tool.updated",
    detail: { title: "Search records", tool_kind: "search", status: "completed", error_code: null },
    containing_span_id: "9",
  },
  finish(11, 9, alphaTool),
  message(12, alphaActOne, "message-alpha-one", alphaText),
  completed(13, alphaActOne, "message-alpha-one", alphaText),
  {
    kind: "instant_occurred", schema_version: 1, run_id: RUN_ID, sequence: "14",
    elapsed_ns: "140", scope: alphaActOne, caused_by: [], instant_kind: "result.accepted",
    detail: { issue: null, error_code: null }, containing_span_id: "6",
  },
  {
    kind: "context_usage_sampled", schema_version: 1, run_id: RUN_ID, sequence: "15",
    elapsed_ns: "150", scope: alphaActOne, caused_by: [], context_used_tokens: "7200",
    context_window_tokens: "16000", cumulative_cost_amount: "0.42",
    cumulative_cost_currency: "USD", sample_origin: "provider", observed_elapsed_ns: "149",
  },
  {
    kind: "act_token_usage_finalized", schema_version: 1, run_id: RUN_ID, sequence: "16",
    elapsed_ns: "160", scope: alphaActOne, caused_by: [], availability: "available",
    source: "acp.prompt_response.usage", unavailable_reason: null,
    provider_total_tokens: "420", input_tokens: "300", output_tokens: "100",
    thought_tokens: "20", cached_read_tokens: "40", cached_write_tokens: "0",
  },
  finish(17, 6, alphaActOne),
  finish(18, 5, alphaOne),
  span(19, "cue.mailbox_wait", alphaTwo),
  finish(20, 19, alphaTwo),
  span(21, "cue.execution", alphaTwo),
  span(22, "act.lifecycle", alphaActTwo, { provider: "fixture", effective_model: "model-b", effective_effort: "medium" }),
  message(23, alphaActTwo, "message-alpha-two", "Alpha is streaming a second cue independently."),
  span(24, "scene.lifecycle", betaScene),
  span(25, "actor.handle_lifetime", betaActor, { display_name: "Builder Beta", actor_type: "BuildActor" }),
  span(26, "cue.mailbox_wait", betaCue),
  finish(27, 26, betaCue),
  span(28, "cue.execution", betaCue),
  span(29, "act.lifecycle", betaAct, { provider: "fixture", effective_model: "model-c", effective_effort: "low" }),
  message(30, betaAct, "message-beta", "Beta works in a separate scene and Actor lane."),
  {
    kind: "counter_sampled", schema_version: 1, run_id: RUN_ID, sequence: "31",
    elapsed_ns: "4096", scope: betaAct, caused_by: [], counter_kind: "agent.turn.active", value: "1",
  },
].map(decodeDiagnosticEvent);
let diagnostics = events.reduce(
  (state, event) => reduceDiagnosticState(state, { type: "event_received", event }),
  createDiagnosticState(RUN_ID, "0"),
);
const bootstrap = {
  document_url: location.href, origin: location.origin,
  api_base_url: new URL("api/v1/", location.href).href,
  identity: { run_id: RUN_ID }, status: { source },
  compatibility: { mode: "interactive", decisions: {}, missingBrowserCapabilities: [] },
};
class Controller {
  listeners = new Set();
  state = {
    phase: source === "archive" ? "archive" : "live",
    connection: source === "archive" ? "archive" : "connected",
    security: "trusted_network", security_scope: "trusted_network",
    outcome: source === "archive" ? "completed" : "running",
    bootstrap, status: bootstrap.status, snapshot: null, diagnostics,
    terminal_reason: null, error: null,
  };
  async start() {}
  stop() {}
  subscribe(listener) { this.listeners.add(listener); return () => this.listeners.delete(listener); }
  publish() { this.listeners.forEach((listener) => listener(this.state)); }
  dispatch(action) {
    diagnostics = reduceDiagnosticState(this.state.diagnostics, action);
    this.state = { ...this.state, diagnostics };
    this.publish();
  }
  connection(value) { this.state = { ...this.state, connection: value }; this.publish(); }
}
const rawViewFixture = __V00_VIEW_FIXTURE__;
const capabilities = __V00_VIEW_CAPABILITIES__;
const viewRecord = decodeViewRecord(rawViewFixture.descriptor);
const viewResponse = decodeViewResponse(rawViewFixture.response, viewRecord);
class ViewClient {
  calls = [];
  pending = [];
  generations = [];
  models = [];
  async loadCatalog() {
    return { api_schema_version: 1, run_id: RUN_ID, capabilities, views: [viewRecord] };
  }
  query(viewId, context) {
    const index = this.calls.length;
    this.calls.push({ viewId, context });
    return new Promise((resolve) => this.pending.push({ index, context, resolve }));
  }
  resolve(index) {
    const pending = this.pending.find((item) => item.index === index);
    if (!pending) throw new Error("unknown pending query " + String(index));
    const generation = freezeViewQueryGeneration(RUN_ID, viewRecord, pending.context);
    const response = {
      ...viewResponse,
      binding: {
        ...viewResponse.binding,
        captured_watermark: pending.context.captured_watermark,
        captured_elapsed_end_ns: pending.context.captured_elapsed_end_ns,
        range_start_ns: pending.context.viewport.start_ns,
        range_end_ns: pending.context.viewport.end_ns,
      },
      bucket_width_ns: generation.expected_bucket_width_ns,
    };
    const model = toTimeSeriesColumnarModel(response);
    this.generations[index] = generation;
    this.models[index] = model;
    pending.resolve({
      generation,
      pagination: freezeViewPagination(viewRecord, capabilities),
      response,
      time_series: model,
    });
  }
  reportRendererFailure(_id, error) {
    return { code: "renderer", message: error instanceof Error ? error.message : String(error) };
  }
  invalidateView() {}
  dispose() {}
}
const controller = new Controller();
const viewClient = new ViewClient();
window.__v00 = {
  appendEvent() {
    const event = decodeDiagnosticEvent(message(
      32, alphaActTwo, "message-paused", "A live message arrived while presentation was paused.",
    ));
    controller.dispatch({ type: "event_received", event });
  },
  setConnection(value) { controller.connection(value); },
  setViewport(start_ns, end_ns) {
    controller.dispatch({ type: "viewport_set", viewport: { start_ns, end_ns } });
    controller.dispatch({ type: "follow_live_set", follow_live: false });
  },
  pendingCount() { return viewClient.pending.length; },
  resolveQuery(index) { viewClient.resolve(index); },
  querySnapshot() {
    return {
      calls: viewClient.calls,
      generations: viewClient.generations,
      models: viewClient.models.map((model) => ({
        bucket_width_ns: model.bucket_width_ns,
        point_count: model.bucket_start_ns.length,
        first_bucket_start_ns: model.bucket_start_ns[0],
      })),
    };
  },
};
render(h(App, {
  liveController: controller,
  viewClientFactory: () => viewClient,
  productionName: source === "archive" ? "Archived visual fixture" : "Live visual fixture",
}), document.querySelector("#app"));
`;

interface DecodedPng {
  readonly width: number;
  readonly height: number;
  readonly rgba: Uint8Array;
}

function paeth(left: number, above: number, upperLeft: number): number {
  const value = left + above - upperLeft;
  const leftDistance = Math.abs(value - left);
  const aboveDistance = Math.abs(value - above);
  const upperLeftDistance = Math.abs(value - upperLeft);
  return leftDistance <= aboveDistance && leftDistance <= upperLeftDistance
    ? left
    : aboveDistance <= upperLeftDistance ? above : upperLeft;
}

function decodePng(bytes: Buffer): DecodedPng {
  expect(bytes.subarray(0, 8)).toEqual(Buffer.from([137, 80, 78, 71, 13, 10, 26, 10]));
  let offset = 8;
  let width = 0;
  let height = 0;
  let colorType = -1;
  const compressed: Buffer[] = [];
  while (offset < bytes.length) {
    const length = bytes.readUInt32BE(offset);
    const kind = bytes.toString("ascii", offset + 4, offset + 8);
    const data = bytes.subarray(offset + 8, offset + 8 + length);
    if (kind === "IHDR") {
      width = data.readUInt32BE(0);
      height = data.readUInt32BE(4);
      expect(data[8]).toBe(8);
      colorType = data[9]!;
      expect(data[10]).toBe(0);
      expect(data[11]).toBe(0);
      expect(data[12]).toBe(0);
    } else if (kind === "IDAT") {
      compressed.push(data);
    } else if (kind === "IEND") {
      break;
    }
    offset += 12 + length;
  }
  const channels = colorType === 6 ? 4 : colorType === 2 ? 3 : 0;
  expect(channels, `unsupported PNG color type ${colorType}`).toBeGreaterThan(0);
  const stride = width * channels;
  const raw = inflateSync(Buffer.concat(compressed));
  expect(raw.byteLength).toBe((stride + 1) * height);
  const decoded = new Uint8Array(stride * height);
  for (let y = 0; y < height; y += 1) {
    const sourceOffset = y * (stride + 1);
    const targetOffset = y * stride;
    const filter = raw[sourceOffset]!;
    if (filter < 0 || filter > 4) {
      throw new Error(`unsupported PNG filter ${filter}`);
    }
    for (let x = 0; x < stride; x += 1) {
      const byte = raw[sourceOffset + x + 1]!;
      const left = x >= channels ? decoded[targetOffset + x - channels]! : 0;
      const above = y > 0 ? decoded[targetOffset + x - stride]! : 0;
      const upperLeft = y > 0 && x >= channels
        ? decoded[targetOffset + x - stride - channels]!
        : 0;
      const predictor = filter === 0 ? 0
        : filter === 1 ? left
          : filter === 2 ? above
            : filter === 3 ? Math.floor((left + above) / 2)
              : paeth(left, above, upperLeft);
      decoded[targetOffset + x] = (byte + predictor) & 0xff;
    }
  }
  const rgba = new Uint8Array(width * height * 4);
  for (let pixel = 0; pixel < width * height; pixel += 1) {
    rgba[pixel * 4] = decoded[pixel * channels]!;
    rgba[pixel * 4 + 1] = decoded[pixel * channels + 1]!;
    rgba[pixel * 4 + 2] = decoded[pixel * channels + 2]!;
    rgba[pixel * 4 + 3] = channels === 4 ? decoded[pixel * channels + 3]! : 255;
  }
  return { width, height, rgba };
}

function comparePixels(actual: Buffer, expected: Buffer): number {
  const left = decodePng(actual);
  const right = decodePng(expected);
  expect([left.width, left.height]).toEqual([right.width, right.height]);
  let changed = 0;
  const threshold = PIXEL_ORACLE.channel_delta;
  for (let pixel = 0; pixel < left.width * left.height; pixel += 1) {
    const offset = pixel * 4;
    if (
      Math.abs(left.rgba[offset]! - right.rgba[offset]!) > threshold
      || Math.abs(left.rgba[offset + 1]! - right.rgba[offset + 1]!) > threshold
      || Math.abs(left.rgba[offset + 2]! - right.rgba[offset + 2]!) > threshold
      || Math.abs(left.rgba[offset + 3]! - right.rgba[offset + 3]!) > threshold
    ) {
      changed += 1;
    }
  }
  return changed / (left.width * left.height);
}

function webkitHostLibrariesAvailable(): boolean {
  if (process.platform !== "linux") {
    return true;
  }
  const result = spawnSync("/sbin/ldconfig", ["-p"], { encoding: "utf8" });
  return result.status !== 0 || ["libgstcodecparsers-1.0.so.0", "libavif.so.13"]
    .every((library) => result.stdout.includes(library));
}

function fixturePlugin(): Plugin {
  const source = FIXTURE_TEMPLATE
    .replace("__V00_VIEW_FIXTURE__", JSON.stringify(VIEW_FIXTURE))
    .replace("__V00_VIEW_CAPABILITIES__", JSON.stringify(VIEW_CAPABILITIES));
  return {
    name: "v00-visual-fixture",
    resolveId(id) {
      return id === "/__v00-entry.js" ? "\0v00-entry.js" : null;
    },
    load(id) {
      return id === "\0v00-entry.js" ? source : null;
    },
    configureServer(server) {
      server.middlewares.use(async (request, response, next) => {
        const pathname = new URL(request.url ?? "/", "http://127.0.0.1").pathname;
        if (pathname !== "/__v00") {
          next();
          return;
        }
        try {
          const html = await server.transformIndexHtml(pathname, String.raw`<!doctype html>
<html lang="en"><head><meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1">
<title>Troupe visual acceptance</title></head><body><main id="app"></main>
<script type="module" src="/__v00-entry.js"></script></body></html>`);
          response.statusCode = 200;
          response.setHeader("Content-Type", "text/html; charset=utf-8");
          response.end(html);
        } catch (error) {
          next(error as Error);
        }
      });
    },
  };
}

async function openFixture(
  page: Page,
  origin: string,
  scenario: VisualScenario,
  viewport: VisualViewportName,
): Promise<void> {
  await page.setViewportSize(VISUAL_VIEWPORTS[viewport]);
  await page.goto(`${origin}/__v00?source=${scenario}`, { waitUntil: "networkidle" });
  await page.addStyleTag({ content: String.raw`
    *, *::before, *::after { animation: none !important; transition: none !important; }
    html, body, button, input, select, textarea { font-family: Arial, sans-serif !important; }
    * { caret-color: transparent !important; }
  ` });
  await expect(page.getByRole("heading", { name: "Troupe Diagnostics" })).toBeVisible();
  await expect(page.locator("canvas.timeline-canvas")).toBeVisible();
  await page.evaluate(() => new Promise<void>((resolveFrame) => {
    requestAnimationFrame(() => requestAnimationFrame(() => resolveFrame()));
  }));
}

async function canvasReport(page: Page): Promise<{
  readonly painted: number;
  readonly rowIdsMatch: boolean;
  readonly backingWidth: number;
  readonly cssWidth: number;
}> {
  return page.evaluate(() => {
    const canvas = document.querySelector<HTMLCanvasElement>("canvas.timeline-canvas");
    const grid = document.querySelector<HTMLElement>("[role=treegrid]");
    if (canvas === null || grid === null) {
      throw new Error("timeline canvas or treegrid is absent");
    }
    const context = canvas.getContext("2d");
    if (context === null) {
      throw new Error("timeline canvas context is absent");
    }
    const pixels = context.getImageData(0, 0, canvas.width, canvas.height).data;
    let painted = 0;
    for (let offset = 0; offset < pixels.length; offset += 4) {
      if (pixels[offset] !== 255 || pixels[offset + 1] !== 255 || pixels[offset + 2] !== 255) {
        painted += 1;
      }
    }
    return {
      painted,
      rowIdsMatch: canvas.dataset.visibleRowIds === grid.dataset.visibleRowIds,
      backingWidth: canvas.width,
      cssWidth: canvas.getBoundingClientRect().width,
    };
  });
}

let server: ViteDevServer | null = null;
let origin = "";

test.skip(
  ({ browserName }) => browserName === "webkit" && !webkitHostLibrariesAvailable(),
  "host lacks the shared libraries required by the pinned WebKit build",
);

test.beforeAll(async ({ browserName }) => {
  const cacheRoot = process.env.TROUPE_GATE_TMP ?? PROJECT_ROOT;
  server = await createServer({
    root: PROJECT_ROOT,
    cacheDir: resolve(cacheRoot, `vite-v00-${browserName}`),
    logLevel: "error",
    server: { host: "127.0.0.1", port: 0, strictPort: false },
    plugins: [fixturePlugin()],
  });
  await server.listen();
  const address = server.httpServer?.address();
  if (address === null || address === undefined || typeof address === "string") {
    throw new Error("V00 fixture did not bind an inet address");
  }
  origin = `http://127.0.0.1:${address.port}`;
});

test.afterAll(async () => {
  await server?.close();
  server = null;
});

for (const viewport of Object.keys(VISUAL_VIEWPORTS) as VisualViewportName[]) {
  for (const scenario of ["active", "archive"] as const) {
    test(`${viewport} ${scenario} remains interactive and matches its fixed pixels`, async ({
      page,
      browserName,
    }) => {
      await openFixture(page, origin, scenario, viewport);
      const canvas = await canvasReport(page);
      expect(canvas.painted).toBeGreaterThan(PIXEL_ORACLE.minimum_canvas_painted_pixels);
      expect(canvas.rowIdsMatch).toBe(true);
      expect(canvas.backingWidth).toBeGreaterThanOrEqual(canvas.cssWidth);

      const geometry = await page.evaluate(() => ({
        scrollWidth: document.body.scrollWidth,
        clientWidth: document.documentElement.clientWidth,
        canvasTop: document.querySelector("canvas")?.getBoundingClientRect().top ?? -1,
        gridTop: document.querySelector("[role=treegrid]")?.getBoundingClientRect().top ?? -1,
      }));
      expect(geometry.scrollWidth).toBeLessThanOrEqual(geometry.clientWidth);
      expect(geometry.gridTop).toBeGreaterThan(geometry.canvasTop);

      if (viewport === "desktop" && scenario === "active") {
        const execution = page.getByLabel("Execution tree");
        await expect(execution.getByRole("button", { name: "Analyst Alpha, actor-alpha" }))
          .toBeVisible();
        await expect(execution.getByRole("button", { name: "Builder Beta, actor-beta" }))
          .toBeVisible();
        await expect(execution.getByRole("button", { name: "Cue cue-one", exact: true }))
          .toBeVisible();
        await expect(execution.getByRole("button", { name: "Cue cue-two", exact: true }))
          .toBeVisible();
        await execution.getByRole("button", { name: "Expand Cue cue-one" }).click();
        await expect(execution.getByRole("button", { name: "Search records, tool-alpha" }))
          .toBeVisible();

        const viewportOutput = page.getByLabel("Timeline viewport");
        const originalStart = await viewportOutput.getAttribute("data-start-ns");
        await page.getByRole("button", { name: "Zoom timeline in" }).click();
        await expect(viewportOutput).not.toHaveAttribute("data-start-ns", originalStart ?? "");
        await page.getByRole("button", { name: "Pan timeline earlier" }).click();
        await page.getByRole("button", { name: "Pan timeline later" }).click();
        await page.getByRole("button", { name: "Follow live timeline" }).click();
        await expect(page.getByRole("button", { name: "Follow live timeline" }))
          .toHaveAttribute("aria-pressed", "true");

        await execution.getByRole("button", { name: "Cue cue-two", exact: true }).click();
        await expect(execution.getByRole("treeitem", { name: /Cue cue-two/ }))
          .toHaveAttribute("aria-selected", "true");
        await page.getByRole("tab", { name: "Agent" }).click();
        const transcript = page.getByLabel("Agent transcript");
        await expect(transcript).toContainText(ALPHA_TEXT);
        await expect(transcript).toContainText("Alpha is streaming a second cue independently");
        await expect(transcript).toContainText("Beta works in a separate scene and Actor lane");
        await expect(transcript).toContainText("Search records");
        await expect(transcript).toContainText("Result accepted");
        await page.getByRole("tab", { name: "Events" }).click();
        await expect(page.getByLabel("Event explorer")).toBeVisible();
        await expect(page.getByLabel("Event inspector")).toBeVisible();
        await execution.getByRole("button", { name: "Cue cue-one", exact: true }).click();
        await page.getByRole("tab", { name: "Usage" }).click();
        await expect(page.getByRole("heading", { name: "Final Act accounting" })).toBeVisible();
        await expect(page.getByRole("region", { name: "Live context" })).toContainText("7,200");
        await page.getByRole("tab", { name: "Views" }).click();
        await expect(page.getByRole("tab", { name: "Queue depth" })).toBeVisible();

        await page.getByRole("tab", { name: "Timeline" }).click();
        await page.getByRole("button", { name: "Pause live presentation" }).click();
        await page.evaluate(() => (
          globalThis as unknown as { __v00: { appendEvent(): void } }
        ).__v00.appendEvent());
        await expect(page.getByLabel("Unseen sequences")).toContainText("1 unseen");
        await page.getByRole("button", { name: "Resume live presentation" }).click();
        await page.evaluate(() => (
          globalThis as unknown as { __v00: { setConnection(value: string): void } }
        ).__v00.setConnection("reconnecting"));
        await expect(page.getByLabel("Run status")).toContainText("Reconnecting");
        await page.evaluate(() => (
          globalThis as unknown as { __v00: { setConnection(value: string): void } }
        ).__v00.setConnection("connected"));
        await page.setViewportSize({ width: 900, height: 650 });
        expect((await canvasReport(page)).painted)
          .toBeGreaterThan(PIXEL_ORACLE.minimum_canvas_painted_pixels);

        await openFixture(page, origin, scenario, viewport);
      }

      const screenshot = await page.screenshot({ animations: "disabled" });
      const file = `${browserName}-${viewport}-${scenario}.png`;
      const baselinePath = resolve(BASELINE_ROOT, file);
      if (process.env.TROUPE_CAPTURE_VISUAL_BASELINES === "1") {
        writeFileSync(baselinePath, screenshot, { flag: "w" });
        return;
      }
      expect(process.env.TROUPE_VISUAL_FORBID_UPDATE).toBe("1");
      const entry = SCREENSHOTS.captures.find((candidate) => candidate.file === file);
      expect(entry, `missing screenshot manifest entry ${file}`).toBeDefined();
      expect(entry!.capture_engine).toBe(browserName);
      const baseline = readFileSync(baselinePath);
      expect(createHash("sha256").update(baseline).digest("hex")).toBe(entry!.sha256);
      const decoded = decodePng(baseline);
      expect([decoded.width, decoded.height]).toEqual([entry!.width, entry!.height]);
      expect(comparePixels(screenshot, baseline)).toBeLessThanOrEqual(
        PIXEL_ORACLE.maximum_changed_pixel_ratio,
      );
    });
  }
}

test("drops an older TimeSeries viewport response after a newer full refetch", async ({ page }) => {
  await openFixture(page, origin, "active", "desktop");
  await page.getByRole("tab", { name: "Views" }).click();
  await expect(page.getByRole("tab", { name: "Queue depth" })).toBeVisible();
  await expect.poll(() => page.evaluate(() => (
    globalThis as unknown as { __v00: { pendingCount(): number } }
  ).__v00.pendingCount())).toBe(1);

  await page.setViewportSize({ width: 1040, height: 720 });
  await page.evaluate(() => (
    globalThis as unknown as { __v00: { setViewport(start: string, end: string): void } }
  ).__v00.setViewport("0", "2048"));
  await expect.poll(() => page.evaluate(() => (
    globalThis as unknown as { __v00: { pendingCount(): number } }
  ).__v00.pendingCount())).toBe(2);
  await page.evaluate(() => (
    globalThis as unknown as { __v00: { resolveQuery(index: number): void } }
  ).__v00.resolveQuery(1));
  await expect(page.locator(".timeseries-renderer")).toBeVisible();
  const boundary = page.locator(".view-panel-boundary");
  const newerIdentity = await boundary.getAttribute("data-query-identity");
  await page.evaluate(() => (
    globalThis as unknown as { __v00: { resolveQuery(index: number): void } }
  ).__v00.resolveQuery(0));
  await page.waitForTimeout(50);
  await expect(boundary).toHaveAttribute("data-query-identity", newerIdentity ?? "");

  const snapshot = await page.evaluate(() => (
    globalThis as unknown as { __v00: { querySnapshot(): unknown } }
  ).__v00.querySnapshot()) as {
    readonly calls: readonly { readonly viewId: string; readonly context: {
      readonly viewport: { readonly start_ns: string; readonly end_ns: string };
    } }[];
    readonly generations: readonly { readonly expected_bucket_width_ns: string }[];
    readonly models: readonly {
      readonly bucket_width_ns: string;
      readonly point_count: number;
      readonly first_bucket_start_ns: string;
    }[];
  };
  expect(snapshot.calls.map((call) => call.viewId)).toEqual(["queue_depth", "queue_depth"]);
  expect(snapshot.calls[1]!.context.viewport).toEqual({ start_ns: "0", end_ns: "2048" });
  expect(snapshot.generations[1]!.expected_bucket_width_ns).toBe("3");
  expect(snapshot.models[1]!.bucket_width_ns).toBe("3");
  expect(snapshot.models[1]!.point_count).toBeGreaterThan(100);
  expect(snapshot.models[1]!.first_bucket_start_ns).toBe("0");
});
