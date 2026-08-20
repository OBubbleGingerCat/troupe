import { mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";

import { expect, test, type Page } from "@playwright/test";
import { createServer, type Plugin, type ViteDevServer } from "vite";


const PROJECT_ROOT = resolve(import.meta.dirname, "../..");
const LOOPBACK_NO_PROXY = "127.0.0.1,localhost,::1";
process.env.NO_PROXY = LOOPBACK_NO_PROXY;
process.env.no_proxy = LOOPBACK_NO_PROXY;

const FIXTURE_SOURCE = String.raw`
import { h, render } from "preact";
import { App } from "/src/app.tsx";
import { ActorTimeline } from "/src/timeline/actor_timeline.tsx";
import { decodeU64 } from "/src/protocol/decimal.ts";
import { createDiagnosticState, reduceDiagnosticState } from "/src/state/reducer.ts";
import { selectCapturedTimelineData, selectProductionTimelineData } from "/src/timeline/production_timeline.ts";
import { COMPLEX_EVENTS, COMPLEX_WATERMARK } from "/tests/fixtures/complex_events.ts";

const historyData = selectCapturedTimelineData(COMPLEX_EVENTS, decodeU64(COMPLEX_WATERMARK), {
  productionName: "Complex timeline fixture",
  connectionLabel: "Archive",
  outcomeLabel: "completed",
});
let state = createDiagnosticState(COMPLEX_EVENTS[0].run_id, decodeU64("0"));
for (const event of COMPLEX_EVENTS) {
  state = reduceDiagnosticState(state, { type: "event_received", event });
}
const data = selectProductionTimelineData(state, {
  productionName: "Complex timeline fixture",
  connection: "connected",
  outcome: "running",
});

if (new URL(location.href).searchParams.has("burst")) {
  const bootstrap = {
    document_url: location.href,
    origin: location.origin,
    api_base_url: new URL("/api/v1/", location.origin).href,
    identity: { run_id: COMPLEX_EVENTS[0].run_id },
    status: { source: "active" },
    compatibility: { mode: "interactive", decisions: {}, missingBrowserCapabilities: [] },
  };
  class BurstController {
    listeners = new Set();
    state = {
      phase: "live",
      connection: "connected",
      security: "trusted_network",
      security_scope: "trusted_network",
      outcome: "running",
      bootstrap,
      status: { source: "active" },
      snapshot: null,
      diagnostics: state,
      terminal_reason: null,
      error: null,
    };
    subscribe(listener) {
      this.listeners.add(listener);
      return () => this.listeners.delete(listener);
    }
    async start() {}
    stop() {}
    dispatch() {}
    publish(index) {
      this.state = {
        ...this.state,
        diagnostics: {
          ...state,
          cursor: {
            ...state.cursor,
            committed_watermark: String(BigInt(COMPLEX_WATERMARK) + BigInt(index)),
          },
        },
      };
      for (const listener of this.listeners) listener(this.state);
    }
  }
  const controller = new BurstController();
  globalThis.__runTimelineBurst = async (count) => {
    await new Promise((resolve) => setTimeout(resolve, 300));
    const watermark = document.querySelector("[title='Run running']");
    let mutations = 0;
    const observer = new MutationObserver(() => { mutations += 1; });
    observer.observe(watermark, { childList: true, characterData: true, subtree: true });
    let maximumHeartbeatDelay = 0;
    let expectedHeartbeat = performance.now() + 25;
    const heartbeat = setInterval(() => {
      const now = performance.now();
      maximumHeartbeatDelay = Math.max(maximumHeartbeatDelay, now - expectedHeartbeat);
      expectedHeartbeat = now + 25;
    }, 25);
    const started = performance.now();
    for (let index = 1; index <= count; index += 1) {
      await new Promise((resolve) => setTimeout(resolve, 0));
      controller.publish(index);
    }
    await new Promise((resolve) => setTimeout(resolve, 500));
    clearInterval(heartbeat);
    observer.disconnect();
    return {
      elapsed: performance.now() - started,
      maximumHeartbeatDelay,
      mutations,
      watermark: watermark?.textContent?.trim() ?? null,
      expectedWatermark: String(BigInt(COMPLEX_WATERMARK) + BigInt(count)),
      domNodes: document.getElementsByTagName("*").length,
      cueTracks: document.querySelectorAll(".cue-track").length,
    };
  };
  render(h(App, { liveController: controller }), document.querySelector("#app"));
} else {
  render(h(ActorTimeline, {
    data,
    historyData,
    historyStatus: "ready",
    livePaused: false,
    unseenCount: 0n,
    onPauseToggle: () => undefined,
  }), document.querySelector("#app"));
}
`;

function fixturePlugin(): Plugin {
  return {
    name: "complex-timeline-geometry-fixture",
    resolveId(id) {
      return id === "/__timeline-geometry-entry.js" ? "\0timeline-geometry-entry.js" : null;
    },
    load(id) {
      return id === "\0timeline-geometry-entry.js" ? FIXTURE_SOURCE : null;
    },
    configureServer(server) {
      server.middlewares.use(async (request, response, next) => {
        const pathname = new URL(request.url ?? "/", "http://127.0.0.1").pathname;
        if (pathname !== "/__timeline-geometry") {
          next();
          return;
        }
        try {
          const html = await server.transformIndexHtml(pathname, String.raw`<!doctype html>
<html lang="en"><head><meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1">
<link rel="icon" href="data:,"><title>Complex timeline geometry</title></head>
<body><main id="app"></main><script type="module" src="/__timeline-geometry-entry.js"></script></body></html>`);
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

interface GeometryIssue {
  readonly actorId: string;
  readonly selector: string;
  readonly top: number;
  readonly bottom: number;
  readonly rowTop: number;
  readonly rowBottom: number;
}

async function openHistory(page: Page, origin: string): Promise<void> {
  await page.goto(`${origin}/__timeline-geometry`, { waitUntil: "networkidle" });
  await expect(page.getByRole("heading", { name: "Troupe Timeline" })).toBeVisible();
  await page.getByRole("button", { name: "History", exact: true }).click();
  await expect(page.getByLabel("History range selection")).toBeVisible();
  await page.locator(".run-overview__scene").first().click();
  await expect(page.locator(".custom-span-track").first()).toBeVisible();
}

test.describe("complex timeline geometry", () => {
  let server: ViteDevServer | null = null;
  let origin = "";
  let cacheRoot: string | null = null;

  test.beforeAll(async () => {
    cacheRoot = mkdtempSync(join(tmpdir(), "troupe-timeline-geometry-"));
    server = await createServer({
      root: PROJECT_ROOT,
      cacheDir: resolve(cacheRoot, "vite"),
      logLevel: "error",
      server: { host: "127.0.0.1", port: 0, strictPort: false },
      plugins: [fixturePlugin()],
    });
    await server.listen();
    const address = server.httpServer?.address();
    if (address === null || address === undefined || typeof address === "string") {
      throw new Error("timeline geometry fixture did not bind an inet address");
    }
    origin = `http://127.0.0.1:${address.port}`;
  });

  test.afterAll(async () => {
    await server?.close();
    server = null;
    if (cacheRoot !== null) {
      rmSync(cacheRoot, { recursive: true, force: true });
      cacheRoot = null;
    }
  });

  test("keeps Cue, Act, deep span, and marker geometry inside each Actor row", async ({ page }) => {
    await openHistory(page, origin);
    const report = await page.evaluate(() => {
      const issues: GeometryIssue[] = [];
      const markerCollisions: unknown[] = [];
      const rows = [...document.querySelectorAll<SVGGElement>(".actor-visual")];
      for (const row of rows) {
        const actorId = row.dataset.actorId ?? "unknown";
        const rowTop = Number(row.dataset.rowTop);
        const rowBottom = rowTop + Number(row.dataset.rowHeight);
        const selectors = [
          ".actor-lifetime-track",
          ".cue-wait-bar",
          ".cue-execution-bar",
          ".act-bar",
          ".custom-span-bar",
          ".event-marker__focus",
        ];
        for (const selector of selectors) {
          for (const element of row.querySelectorAll<SVGGraphicsElement>(selector)) {
            const bounds = element.getBBox();
            const top = bounds.y;
            const bottom = bounds.y + bounds.height;
            if (top < rowTop - 0.5 || bottom > rowBottom + 0.5) {
              issues.push({ actorId, selector, top, bottom, rowTop, rowBottom });
            }
          }
        }
      }
      for (const marker of document.querySelectorAll<SVGGElement>(".event-marker")) {
        const cue = marker.closest<SVGGElement>(".cue-track");
        const spans = cue === null
          ? []
          : [...cue.querySelectorAll<SVGGraphicsElement>(".custom-span-bar")];
        const hit = marker.querySelector<SVGGraphicsElement>(".event-marker__hit");
        if (spans.length === 0 || hit === null) {
          continue;
        }
        const spanBottom = Math.max(...spans.map((span) => {
          const bounds = span.getBBox();
          return bounds.y + bounds.height;
        }));
        const markerBox = hit.getBBox();
        if (markerBox.y < spanBottom + 2) {
          markerCollisions.push({
            cueId: cue?.dataset.cueId,
            event: marker.dataset.eventLabel,
            spanBottom,
            markerTop: markerBox.y,
          });
        }
      }
      for (const cue of document.querySelectorAll<SVGGElement>(".cue-track")) {
        const system = cue.querySelector<SVGGElement>(".event-marker:not(.custom-event-marker)");
        const custom = cue.querySelector<SVGGElement>(".custom-event-marker");
        const systemHit = system?.querySelector<SVGGraphicsElement>(".event-marker__hit");
        const customHit = custom?.querySelector<SVGGraphicsElement>(".event-marker__hit");
        if (systemHit !== null && systemHit !== undefined && customHit !== null && customHit !== undefined) {
          const systemBox = systemHit.getBBox();
          const customBox = customHit.getBBox();
          if (customBox.y < systemBox.y + systemBox.height + 2) {
            markerCollisions.push({
              cueId: cue.dataset.cueId,
              systemBottom: systemBox.y + systemBox.height,
              customTop: customBox.y,
            });
          }
        }
      }
      return {
        issues,
        markerCollisions,
        actorRows: rows.length,
        cueTracks: document.querySelectorAll(".cue-track").length,
        deepSpans: [...document.querySelectorAll<SVGGElement>(".custom-span-track")]
          .filter((span) => {
            const parentId = span.dataset.parentSpanId;
            if (parentId === undefined) {
              return false;
            }
            const parent = document.querySelector<SVGGElement>(`.custom-span-track[data-span-id='${parentId}']`);
            return parent?.dataset.parentSpanId !== undefined;
          }).length,
        customEvents: document.querySelectorAll(".custom-event-marker").length,
      };
    });

    expect(report.actorRows).toBeGreaterThanOrEqual(4);
    expect(report.cueTracks).toBeGreaterThanOrEqual(5);
    expect(report.deepSpans).toBeGreaterThan(0);
    expect(report.customEvents).toBeGreaterThan(0);
    expect(report.issues, JSON.stringify(report.issues, null, 2)).toEqual([]);
    expect(report.markerCollisions, JSON.stringify(report.markerCollisions, null, 2)).toEqual([]);
    await expect(page.locator(".actor-visual[data-actor-id='actor-dynamic-1']")).toHaveCount(1);
  });

  test("keeps only current persistent Actors and recent Cues in Live", async ({ page }) => {
    await page.goto(`${origin}/__timeline-geometry`, { waitUntil: "networkidle" });
    await expect(page.getByRole("heading", { name: "Troupe Timeline" })).toBeVisible();
    const actorIds = await page.locator(".actor-visual").evaluateAll((actors) => (
      actors.map((actor) => (actor as SVGGElement).dataset.actorId)
    ));
    expect(actorIds).toEqual(["actor-ingest", "actor-review", "actor-publish"]);
    await expect(page.locator(".actor-visual[data-actor-id^='actor-dynamic-']")).toHaveCount(0);
    await expect(page.locator(".cue-track[data-cue-id='cue-1-ingest-primary']")).toHaveCount(0);
    await expect(page.locator(".cue-track[data-cue-id='cue-48-ingest-primary']")).toHaveCount(1);
  });

  test("keeps dense Scene labels readable and inside their Scene bars", async ({ page }) => {
    await page.goto(`${origin}/__timeline-geometry`, { waitUntil: "networkidle" });
    await expect(page.getByRole("heading", { name: "Troupe Timeline" })).toBeVisible();
    const report = await page.evaluate(() => {
      const labels = [...document.querySelectorAll<SVGGElement>(".scene-band")]
        .flatMap((band) => {
          const label = band.querySelector<SVGTextElement>(".scene-label-svg");
          const bar = band.querySelectorAll<SVGRectElement>("rect")[1];
          if (label === null || bar === undefined) {
            return [];
          }
          const labelBox = label.getBoundingClientRect();
          const barBox = bar.getBoundingClientRect();
          return [{
            id: band.dataset.sceneId,
            left: labelBox.left,
            right: labelBox.right,
            barLeft: barBox.left,
            barRight: barBox.right,
          }];
        });
      const overlaps: unknown[] = [];
      for (let leftIndex = 0; leftIndex < labels.length; leftIndex += 1) {
        for (let rightIndex = leftIndex + 1; rightIndex < labels.length; rightIndex += 1) {
          const left = labels[leftIndex]!;
          const right = labels[rightIndex]!;
          if (left.left < right.right - 0.5 && right.left < left.right - 0.5) {
            overlaps.push({ left: left.id, right: right.id });
          }
        }
      }
      return { labels, overlaps };
    });

    expect(report.labels.length).toBeGreaterThan(0);
    expect(report.labels.every((label) => (
      label.left >= label.barLeft - 0.5 && label.right <= label.barRight + 0.5
    ))).toBe(true);
    expect(report.overlaps, JSON.stringify(report.overlaps, null, 2)).toEqual([]);
  });

  test("coalesces a high-rate Live burst without blocking or growing the DOM", async ({ page }) => {
    await page.goto(`${origin}/__timeline-geometry?burst=1`, { waitUntil: "networkidle" });
    await expect(page.getByRole("heading", { name: "Troupe Timeline" })).toBeVisible();
    const report = await page.evaluate(async () => {
      const run = (globalThis as unknown as {
        __runTimelineBurst: (count: number) => Promise<{
          elapsed: number;
          maximumHeartbeatDelay: number;
          mutations: number;
          watermark: string | null;
          expectedWatermark: string;
          domNodes: number;
          cueTracks: number;
        }>;
      }).__runTimelineBurst;
      return run(300);
    });

    expect(report.watermark).toBe(report.expectedWatermark);
    expect(report.mutations).toBeLessThanOrEqual(10);
    expect(report.maximumHeartbeatDelay).toBeLessThan(1_000);
    expect(report.elapsed).toBeLessThan(5_000);
    expect(report.domNodes).toBeLessThan(10_000);
    expect(report.cueTracks).toBeGreaterThan(0);
  });

  test("shares row positions between labels and plot and separates overlapping Cue lanes", async ({ page }) => {
    await openHistory(page, origin);
    const report = await page.evaluate(() => {
      const rowMismatches: unknown[] = [];
      for (const visual of document.querySelectorAll<SVGGElement>(".actor-visual")) {
        const actorId = visual.dataset.actorId ?? "";
        const label = document.querySelector<HTMLElement>(`.actor-label[data-actor-id='${actorId}']`);
        if (
          label === null
          || label.dataset.rowTop !== visual.dataset.rowTop
          || label.dataset.rowHeight !== visual.dataset.rowHeight
        ) {
          rowMismatches.push({ actorId, visual: visual.dataset, label: label?.dataset ?? null });
        }
      }

      const cueIntervals = [...document.querySelectorAll<SVGGElement>(".cue-track")].map((cue) => {
        const wait = cue.querySelector<SVGRectElement>(".cue-wait-bar");
        const execute = cue.querySelector<SVGRectElement>(".cue-execution-bar");
        const x = Number(wait?.getAttribute("x") ?? execute?.getAttribute("x") ?? 0);
        const waitEnd = wait === null ? x : Number(wait.getAttribute("x")) + Number(wait.getAttribute("width"));
        const executeEnd = execute === null
          ? waitEnd
          : Number(execute.getAttribute("x")) + Number(execute.getAttribute("width"));
        return {
          actorId: cue.closest<SVGGElement>(".actor-visual")?.dataset.actorId ?? "",
          cueId: cue.dataset.cueId ?? "",
          lane: Number(cue.dataset.cueLane),
          start: x,
          end: Math.max(waitEnd, executeEnd),
        };
      });
      const overlaps: unknown[] = [];
      for (let leftIndex = 0; leftIndex < cueIntervals.length; leftIndex += 1) {
        for (let rightIndex = leftIndex + 1; rightIndex < cueIntervals.length; rightIndex += 1) {
          const left = cueIntervals[leftIndex]!;
          const right = cueIntervals[rightIndex]!;
          if (
            left.actorId === right.actorId
            && left.lane === right.lane
            && left.start < right.end - 0.5
            && right.start < left.end - 0.5
          ) {
            overlaps.push({ left, right });
          }
        }
      }
      return { rowMismatches, overlaps };
    });

    expect(report.rowMismatches, JSON.stringify(report.rowMismatches, null, 2)).toEqual([]);
    expect(report.overlaps, JSON.stringify(report.overlaps, null, 2)).toEqual([]);
  });

  test("maps known Cue, Act, span, and event times to independent x coordinates", async ({ page }) => {
    await openHistory(page, origin);
    const report = await page.evaluate(() => {
      const cue = document.querySelector<SVGGElement>(".cue-track[data-cue-id='cue-1-ingest-primary']");
      const svg = cue?.ownerSVGElement;
      if (cue === null || svg === null || svg === undefined) {
        throw new Error("known fixture Cue is missing");
      }
      const width = svg.viewBox.baseVal.width;
      const expectedX = (time: number): number => 14 + (time / 10) * (width - 28);
      const rect = (selector: string): { x: number; end: number } => {
        const element = cue.querySelector<SVGRectElement>(selector);
        if (element === null) {
          throw new Error(`fixture element is missing: ${selector}`);
        }
        const x = Number(element.getAttribute("x"));
        return { x, end: x + Number(element.getAttribute("width")) };
      };
      const wait = rect(".cue-wait-bar");
      const execution = rect(".cue-execution-bar");
      const act = rect(".act-track[data-act-id='act-1-ingest-primary'] .act-bar");
      const spans = [...cue.querySelectorAll<SVGRectElement>(".custom-span-bar")]
        .slice(0, 3)
        .map((span) => {
          const x = Number(span.getAttribute("x"));
          return { x, end: x + Number(span.getAttribute("width")) };
        });
      const marker = cue.querySelector<SVGLineElement>(
        ".custom-event-marker[data-event-label='example.operation_ready'] .event-marker__anchor",
      );
      if (spans.length !== 3 || marker === null) {
        throw new Error("known fixture nested telemetry is missing");
      }
      const actual = [
        wait.x,
        wait.end,
        execution.x,
        execution.end,
        act.x,
        act.end,
        spans[0]!.x,
        spans[0]!.end,
        spans[1]!.x,
        spans[1]!.end,
        spans[2]!.x,
        spans[2]!.end,
        Number(marker.getAttribute("x1")),
      ];
      const expected = [1, 2, 2, 6, 2.2, 5.85, 2.35, 5.65, 2.55, 3.55, 2.75, 3.1, 2.85]
        .map(expectedX);
      return {
        errors: actual.map((value, index) => Math.abs(value - expected[index]!)),
        containment: {
          waitJoinsExecution: Math.abs(wait.end - execution.x),
          actInsideExecution: act.x >= execution.x && act.end <= execution.end,
          outerInsideAct: spans[0]!.x >= act.x && spans[0]!.end <= act.end,
          middleInsideOuter: spans[1]!.x >= spans[0]!.x && spans[1]!.end <= spans[0]!.end,
          innerInsideMiddle: spans[2]!.x >= spans[1]!.x && spans[2]!.end <= spans[1]!.end,
          eventInsideInner: actual[12]! >= spans[2]!.x && actual[12]! <= spans[2]!.end,
        },
      };
    });

    expect(Math.max(...report.errors)).toBeLessThan(0.75);
    expect(report.containment.waitJoinsExecution).toBeLessThan(0.1);
    expect(report.containment).toEqual({
      waitJoinsExecution: expect.any(Number),
      actInsideExecution: true,
      outerInsideAct: true,
      middleInsideOuter: true,
      innerInsideMiddle: true,
      eventInsideInner: true,
    });
  });

  test("marks a future Cue wait pending at an earlier History playhead", async ({ page }) => {
    await openHistory(page, origin);
    const playhead = page.getByRole("slider", { name: "History playhead" });
    await playhead.fill("0");
    const futureWait = page.locator(".cue-wait-track").first();
    await futureWait.focus();
    await expect(page.getByRole("tooltip")).toContainText("Pending");
  });
});
