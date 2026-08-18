import { spawnSync } from "node:child_process";
import { mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";

import AxeBuilder from "@axe-core/playwright";
import { expect, test, type Browser, type Page } from "@playwright/test";
import { createServer, type Plugin, type ViteDevServer } from "vite";


const PROJECT_ROOT = resolve(import.meta.dirname, "../../..");
const LOOPBACK_NO_PROXY = "127.0.0.1,localhost,::1";
process.env.NO_PROXY = LOOPBACK_NO_PROXY;
process.env.no_proxy = LOOPBACK_NO_PROXY;

const FIXTURE_SOURCE = String.raw`
import { h, render } from "preact";
import { App } from "/src/app.tsx";
import { decodeDiagnosticEvent } from "/src/protocol/event.ts";
import { createDiagnosticState, reduceDiagnosticState } from "/src/state/reducer.ts";

const RUN_ID = "12345678-1234-4234-9234-123456789abc";
const mode = new URL(location.href).searchParams.get("mode") || "active";
const scope = (cue, act = null) => ({
  scene_id: "scene-accessibility",
  actor_id: "actor-keyboard",
  cue_id: cue,
  effect_id: null,
  act_id: act,
  tool_call_id: null,
  session_generation: "1",
});
const sceneScope = { ...scope(null), actor_id: null };
const events = [
  {
    kind: "span_started", schema_version: 1, run_id: RUN_ID, sequence: "1",
    elapsed_ns: "10000000000", scope: sceneScope, caused_by: [], span_kind: "scene.lifecycle",
    detail: {}, parent_span_id: null,
  },
  {
    kind: "span_started", schema_version: 1, run_id: RUN_ID, sequence: "2",
    elapsed_ns: "20000000000", scope: scope(null), caused_by: [], span_kind: "actor.handle_lifetime",
    detail: {
      display_name: "Keyboard operator with an intentionally long descriptive label that must wrap safely",
      actor_type: "AccessibilityActor",
    }, parent_span_id: null,
  },
  {
    kind: "span_started", schema_version: 1, run_id: RUN_ID, sequence: "3",
    elapsed_ns: "30000000000", scope: scope("cue-one"), caused_by: [], span_kind: "cue.mailbox_wait",
    detail: {}, parent_span_id: null,
  },
  {
    kind: "span_finished", schema_version: 1, run_id: RUN_ID, sequence: "4",
    elapsed_ns: "40000000000", scope: scope("cue-one"), caused_by: [], span_id: "3",
    outcome: "completed", error_code: null,
  },
  {
    kind: "span_started", schema_version: 1, run_id: RUN_ID, sequence: "5",
    elapsed_ns: "50000000000", scope: scope("cue-one"), caused_by: [], span_kind: "cue.execution",
    detail: {}, parent_span_id: null,
  },
  {
    kind: "span_started", schema_version: 1, run_id: RUN_ID, sequence: "6",
    elapsed_ns: "60000000000", scope: scope("cue-one", "act-one"), caused_by: [], span_kind: "act.lifecycle",
    detail: { provider: "fixture", effective_model: "model-a", effective_effort: "medium" },
    parent_span_id: null,
  },
  {
    kind: "agent_message_delta", schema_version: 1, run_id: RUN_ID, sequence: "7",
    elapsed_ns: "70000000000", scope: scope("cue-one", "act-one"), caused_by: [],
    message_id: "message-accessible", source_message_id: null,
    text_delta: "A long diagnostic message remains plain, readable, and contained on a narrow mobile viewport. ".repeat(6),
  },
].map(decodeDiagnosticEvent);

let diagnostics = events.reduce(
  (state, event) => reduceDiagnosticState(state, { type: "event_received", event }),
  createDiagnosticState(RUN_ID, "0"),
);
const source = mode === "archive" ? "archive" : "active";
const compatibility = mode === "compatibility";
const bootstrap = {
  document_url: location.href,
  origin: location.origin,
  api_base_url: new URL("api/v1/", location.href).href,
  identity: { run_id: RUN_ID },
  status: { source },
  compatibility: {
    mode: compatibility ? "static" : "interactive",
    decisions: {},
    missingBrowserCapabilities: compatibility ? ["EventSource"] : [],
  },
};
class Controller {
  listeners = new Set();
  state = {
    phase: compatibility ? "compatibility" : source === "archive" ? "archive" : "live",
    connection: compatibility ? "offline" : source === "archive" ? "archive" : "connected",
    security: "trusted_network",
    security_scope: "trusted_network",
    outcome: source === "archive" ? "completed" : "running",
    bootstrap,
    status: compatibility ? null : bootstrap.status,
    snapshot: null,
    diagnostics: compatibility ? null : diagnostics,
    terminal_reason: null,
    error: null,
  };
  async start() {}
  stop() {}
  subscribe(listener) { this.listeners.add(listener); return () => this.listeners.delete(listener); }
  dispatch(action) {
    if (this.state.diagnostics === null) return;
    diagnostics = reduceDiagnosticState(this.state.diagnostics, action);
    this.state = { ...this.state, diagnostics };
    this.listeners.forEach((listener) => listener(this.state));
  }
}
render(h(App, {
  liveController: new Controller(),
  productionName: "Accessibility production",
  historyFetch: async () => new Response(JSON.stringify({
    api_schema_version: 1,
    run_id: RUN_ID,
    captured_watermark: "7",
    events,
    next_after: null,
  }), { status: 200, headers: { "content-type": "application/json" } }),
}), document.querySelector("#app"));
`;

function webkitHostLibrariesAvailable(): boolean {
  if (process.platform !== "linux") {
    return true;
  }
  const result = spawnSync("/sbin/ldconfig", ["-p"], { encoding: "utf8" });
  return result.status !== 0 || ["libgstcodecparsers-1.0.so.0", "libavif.so.13"]
    .every((library) => result.stdout.includes(library));
}

function fixturePlugin(): Plugin {
  return {
    name: "v04-accessibility-fixture",
    resolveId(id) {
      return id === "/__v04-entry.js" ? "\0v04-entry.js" : null;
    },
    load(id) {
      return id === "\0v04-entry.js" ? FIXTURE_SOURCE : null;
    },
    configureServer(server) {
      server.middlewares.use(async (request, response, next) => {
        const pathname = new URL(request.url ?? "/", "http://127.0.0.1").pathname;
        if (pathname !== "/__v04") {
          next();
          return;
        }
        try {
          const html = await server.transformIndexHtml(pathname, String.raw`<!doctype html>
<html lang="en"><head><meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1">
<link rel="icon" href="data:,"><title>Troupe accessibility test</title></head><body><main id="app"></main>
<script type="module" src="/__v04-entry.js"></script></body></html>`);
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

async function visibleApp(page: Page, origin: string, mode: string): Promise<void> {
  await page.goto(`${origin}/__v04?mode=${mode}`, { waitUntil: "networkidle" });
  await expect(page.getByRole("heading", {
    name: mode === "compatibility" ? "Troupe Diagnostics" : "Troupe Timeline",
  })).toBeVisible();
}

function registerAccessibilityTests(): void {
  let server: ViteDevServer | null = null;
  let origin = "";
  let ownedCacheRoot: string | null = null;

  test.skip(
    ({ browserName }) => browserName === "webkit" && !webkitHostLibrariesAvailable(),
    "host lacks the shared libraries required by the pinned WebKit build",
  );

  test.beforeAll(async ({ browserName }) => {
    const cacheRoot = mkdtempSync(join(tmpdir(), "troupe-accessibility-vite-"));
    ownedCacheRoot = cacheRoot;
    server = await createServer({
      root: PROJECT_ROOT,
      cacheDir: resolve(cacheRoot, `vite-v04-${browserName}`),
      logLevel: "error",
      server: { host: "127.0.0.1", port: 0, strictPort: false },
      plugins: [fixturePlugin()],
    });
    await server.listen();
    const address = server.httpServer?.address();
    if (address === null || address === undefined || typeof address === "string") {
      throw new Error("accessibility fixture did not bind an inet address");
    }
    origin = `http://127.0.0.1:${address.port}`;
  });

  test.afterAll(async () => {
    await server?.close();
    server = null;
    if (ownedCacheRoot !== null) {
      rmSync(ownedCacheRoot, { recursive: true, force: true });
      ownedCacheRoot = null;
    }
  });

  test("active and archive surfaces have no unapproved serious axe violation", async ({ page }) => {
    for (const mode of ["active", "archive"]) {
      await visibleApp(page, origin, mode);
      const results = await new AxeBuilder({ page }).analyze();
      const blocking = results.violations.filter((violation) => (
        violation.impact === "critical" || violation.impact === "serious"
      ));
      expect(blocking, JSON.stringify(blocking, null, 2)).toEqual([]);
      await expect(page.getByRole("group", { name: "Timeline mode" })).toBeVisible();
      await expect(page.getByLabel("Actor-centered timeline")).toBeVisible();
      const lifetime = page.locator(".actor-lifetime-track").first();
      await expect(lifetime).toBeVisible();
      await expect(lifetime).toHaveAttribute("aria-label", /Actor lifetime/);
      await expect(page.locator(".actor-lifecycle-marker[data-event='created']").first())
        .toHaveAttribute("aria-label", /Actor created/);
      await expect(page.getByRole("tablist")).toHaveCount(0);
      await expect(page.locator("canvas, [role='treegrid']")).toHaveCount(0);
    }
  });

  test("compatibility state stays semantic and omits interactive surfaces", async ({ page }) => {
    await visibleApp(page, origin, "compatibility");
    await expect(page.getByRole("status", { name: "Compatibility status" }))
      .toContainText("Required browser capabilities are unavailable");
    await expect(page.getByRole("group", { name: "Timeline mode" })).toHaveCount(0);
    await expect(page.locator(".actor-lifetime-track")).toHaveCount(0);
  });

  test("keyboard traverses timeline modes, lifecycles, and History controls", async ({ page }) => {
    await visibleApp(page, origin, "active");
    const live = page.getByRole("button", { name: "Live", exact: true });
    await live.focus();
    await expect(live).toHaveAttribute("aria-pressed", "true");
    const history = page.getByRole("button", { name: "History", exact: true });
    await history.focus();
    await page.keyboard.press("Enter");
    await expect(history).toBeFocused();
    await expect(history).toHaveAttribute("aria-pressed", "true");
    await expect(page.getByLabel("History range selection")).toBeVisible();

    const lifetime = page.locator(".actor-lifetime-track").first();
    await lifetime.focus();
    await expect(lifetime).toBeFocused();
    await expect(page.getByRole("tooltip")).toContainText("Actor lifetime");
    await expect(page.getByRole("tooltip")).toContainText("AccessibilityActor");

    const created = page.locator(".actor-lifecycle-marker[data-event='created']").first();
    await created.focus();
    await expect(created).toBeFocused();
    await expect(page.getByRole("tooltip")).toContainText("Actor created");

    const play = page.getByRole("button", { name: "Play History range" });
    await expect(play).toBeEnabled();
    await play.focus();
    await page.keyboard.press("Enter");
    const pause = page.getByRole("button", { name: "Pause History playback" });
    await expect(pause).toBeFocused();
    await expect(pause).toHaveAttribute("aria-pressed", "true");
    const doubleSpeed = page.getByRole("button", { name: "2x" });
    await doubleSpeed.focus();
    await page.keyboard.press("Enter");
    await expect(doubleSpeed).toHaveAttribute("aria-pressed", "true");

    for (const name of ["History range start", "History range end", "History playhead"]) {
      const slider = page.getByRole("slider", { name });
      await slider.focus();
      await expect(slider).toBeFocused();
    }

    const actor = page.getByRole("button", { name: /Keyboard operator/ }).first();
    await actor.focus();
    await page.keyboard.press("Enter");
    await expect(page.getByRole("complementary", {
      name: "Timeline selection",
      exact: true,
    })).toContainText("Keyboard operator with an intentionally long descriptive label");
  });

  test("touch controls remain operable and long content stays inside mobile geometry", async ({
    browser,
  }: { browser: Browser }) => {
    const context = await browser.newContext({
      viewport: { width: 390, height: 844 },
      hasTouch: true,
      deviceScaleFactor: 1,
    });
    const page = await context.newPage();
    try {
      await visibleApp(page, origin, "active");
      const history = page.getByRole("button", { name: "History", exact: true });
      await history.tap();
      await expect(history).toHaveAttribute("aria-pressed", "true");
      const play = page.getByRole("button", { name: "Play History range" });
      await expect(play).toBeEnabled();
      await play.tap();
      await expect(page.getByRole("button", { name: "Pause History playback" })).toBeFocused();
      const lifetime = page.locator(".actor-lifetime-track").first();
      await lifetime.focus();
      await expect(page.getByRole("tooltip")).toContainText("Actor lifetime");
      const geometry = await page.evaluate(() => ({
        bodyWidth: document.body.scrollWidth,
        viewportWidth: document.documentElement.clientWidth,
        clipped: [...document.querySelectorAll<HTMLElement>(
          ".control-strip button, .control-strip select, .history-controls input, .timeline-inspector button",
        )]
          .filter((element) => {
            const rect = element.getBoundingClientRect();
            return rect.right > innerWidth + 1 || rect.left < -1;
          }).length,
        timelineScrollable: (() => {
          const timeline = document.querySelector<HTMLElement>(".timeline-scroll");
          return timeline !== null && timeline.scrollWidth > timeline.clientWidth;
        })(),
        overflowing: [...document.querySelectorAll<HTMLElement>("body *")]
          .filter((element) => {
            const rect = element.getBoundingClientRect();
            return rect.right > innerWidth + 1 || rect.left < -1;
          })
          .slice(0, 20)
          .map((element) => ({
            element: `${element.tagName.toLowerCase()}.${element.className}`,
            rect: element.getBoundingClientRect().toJSON(),
          })),
      }));
      expect(
        geometry.bodyWidth,
        JSON.stringify(geometry.overflowing, null, 2),
      ).toBeLessThanOrEqual(geometry.viewportWidth);
      expect(geometry.clipped).toBe(0);
      expect(geometry.timelineScrollable).toBe(true);
    } finally {
      await context.close();
    }
  });
}


registerAccessibilityTests();
