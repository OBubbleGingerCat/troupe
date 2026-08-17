import { spawnSync } from "node:child_process";
import { readFileSync } from "node:fs";
import { resolve } from "node:path";

import AxeBuilder from "@axe-core/playwright";
import { expect, test, type Browser, type Page } from "@playwright/test";
import { createServer, type Plugin, type ViteDevServer } from "vite";


const PROJECT_ROOT = resolve(import.meta.dirname, "../../..");
const ALLOWLIST = JSON.parse(
  readFileSync(resolve(import.meta.dirname, "axe-allowlist.json"), "utf8"),
) as { readonly allowed_rule_ids: readonly string[] };
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
    elapsed_ns: "10", scope: sceneScope, caused_by: [], span_kind: "scene.lifecycle",
    detail: {}, parent_span_id: null,
  },
  {
    kind: "span_started", schema_version: 1, run_id: RUN_ID, sequence: "2",
    elapsed_ns: "20", scope: scope(null), caused_by: [], span_kind: "actor.handle_lifetime",
    detail: {
      display_name: "Keyboard operator with an intentionally long descriptive label that must wrap safely",
      actor_type: "AccessibilityActor",
    }, parent_span_id: null,
  },
  {
    kind: "span_started", schema_version: 1, run_id: RUN_ID, sequence: "3",
    elapsed_ns: "30", scope: scope("cue-one"), caused_by: [], span_kind: "cue.mailbox_wait",
    detail: {}, parent_span_id: null,
  },
  {
    kind: "span_finished", schema_version: 1, run_id: RUN_ID, sequence: "4",
    elapsed_ns: "40", scope: scope("cue-one"), caused_by: [], span_id: "3",
    outcome: "completed", error_code: null,
  },
  {
    kind: "span_started", schema_version: 1, run_id: RUN_ID, sequence: "5",
    elapsed_ns: "50", scope: scope("cue-one"), caused_by: [], span_kind: "cue.execution",
    detail: {}, parent_span_id: null,
  },
  {
    kind: "span_started", schema_version: 1, run_id: RUN_ID, sequence: "6",
    elapsed_ns: "60", scope: scope("cue-one", "act-one"), caused_by: [], span_kind: "act.lifecycle",
    detail: { provider: "fixture", effective_model: "model-a", effective_effort: "medium" },
    parent_span_id: null,
  },
  {
    kind: "agent_message_delta", schema_version: 1, run_id: RUN_ID, sequence: "7",
    elapsed_ns: "70", scope: scope("cue-one", "act-one"), caused_by: [],
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
const emptyViews = () => ({
  loadCatalog: async () => ({
    api_schema_version: 1, run_id: RUN_ID, capabilities: {}, views: [],
  }),
  query: async () => { throw new Error("empty fixture has no views"); },
  reportRendererFailure: (_id, error) => error,
  invalidateView() {}, dispose() {},
});
render(h(App, {
  liveController: new Controller(),
  viewClientFactory: emptyViews,
  productionName: "Accessibility production",
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
<title>Troupe accessibility acceptance</title></head><body><main id="app"></main>
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
  await expect(page.getByRole("heading", { name: "Troupe Diagnostics" })).toBeVisible();
}

export function registerAccessibilityAcceptance(): void {
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
      cacheDir: resolve(cacheRoot, `vite-v04-${browserName}`),
      logLevel: "error",
      server: { host: "127.0.0.1", port: 0, strictPort: false },
      plugins: [fixturePlugin()],
    });
    await server.listen();
    const address = server.httpServer?.address();
    if (address === null || address === undefined || typeof address === "string") {
      throw new Error("V04 fixture did not bind an inet address");
    }
    origin = `http://127.0.0.1:${address.port}`;
  });

  test.afterAll(async () => {
    await server?.close();
    server = null;
  });

  test("active and archive surfaces have no unapproved serious axe violation", async ({ page }) => {
    for (const mode of ["active", "archive"]) {
      await visibleApp(page, origin, mode);
      const results = await new AxeBuilder({ page }).analyze();
      const blocking = results.violations.filter((violation) => (
        (violation.impact === "critical" || violation.impact === "serious")
        && !ALLOWLIST.allowed_rule_ids.includes(violation.id)
      ));
      expect(blocking, JSON.stringify(blocking, null, 2)).toEqual([]);
      const treegrid = page.getByRole("treegrid", { name: "Production timeline" });
      const canvas = page.locator("canvas.timeline-canvas");
      await expect(treegrid).toBeVisible();
      await expect(canvas).toHaveAttribute("aria-hidden", "true");
      expect(await treegrid.getAttribute("data-visible-row-ids"))
        .toBe(await canvas.getAttribute("data-visible-row-ids"));
      const rows = treegrid.getByRole("row");
      expect(await rows.count()).toBeGreaterThan(0);
      for (let index = 0; index < await rows.count(); index += 1) {
        const row = rows.nth(index);
        await expect(row).toHaveAttribute("aria-level", /\d+/);
        const kind = await row.getAttribute("data-kind");
        expect(kind).not.toBeNull();
        expect(await row.getAttribute("aria-label")).toContain(`, ${kind}, `);
      }
    }
  });

  test("compatibility state stays semantic and omits interactive surfaces", async ({ page }) => {
    await visibleApp(page, origin, "compatibility");
    await expect(page.getByRole("status", { name: "Compatibility status" }))
      .toContainText("Required browser capabilities are unavailable");
    await expect(page.getByRole("tablist", { name: "Primary diagnostics views" })).toHaveCount(0);
    await expect(page.getByRole("treegrid", { name: "Production timeline" })).toHaveCount(0);
  });

  test("keyboard traverses tree, tabs, and viewport controls without losing focus", async ({ page }) => {
    await visibleApp(page, origin, "active");
    const timelineTab = page.getByRole("tab", { name: "Timeline" });
    await timelineTab.focus();
    await page.keyboard.press("ArrowRight");
    await expect(page.getByRole("tab", { name: "Agent" })).toHaveAttribute("aria-selected", "true");
    await expect(page.getByRole("tab", { name: "Agent" })).toBeFocused();
    await page.keyboard.press("End");
    await expect(page.getByRole("tab", { name: "Views" })).toBeFocused();
    await page.keyboard.press("Home");
    await expect(timelineTab).toBeFocused();
    await page.keyboard.press("Enter");

    const treegrid = page.getByRole("treegrid", { name: "Production timeline" });
    const canvas = page.locator("canvas.timeline-canvas");
    const firstRow = treegrid.getByRole("row").first();
    await firstRow.focus();
    await expect(firstRow).not.toHaveAttribute("aria-expanded");

    await page.keyboard.press("ArrowDown");
    const focusedScene = page.locator("[role=row]:focus");
    await expect(focusedScene).toHaveAttribute("aria-label", /Scene scene-accessibility, scene, running/);
    await expect(focusedScene).not.toHaveAttribute("aria-expanded");

    await page.keyboard.press("ArrowDown");
    await expect(page.locator("[role=row]:focus"))
      .toHaveAttribute("aria-label", "Actor actor-keyboard, actor, running");
    await expect(page.locator("[role=row]:focus")).not.toHaveAttribute("aria-expanded");
    await page.keyboard.press("ArrowDown");
    const focusedCue = page.locator("[role=row]:focus");
    await expect(focusedCue).toHaveAttribute("aria-label", /Cue cue-one, cue, running/);
    await expect(focusedCue).toHaveAttribute("aria-expanded", "false");
    await page.keyboard.press("ArrowRight");
    await expect(focusedCue).toHaveAttribute("aria-expanded", "true");
    expect(await treegrid.getAttribute("data-visible-row-ids"))
      .toBe(await canvas.getAttribute("data-visible-row-ids"));
    await page.keyboard.press("End");
    const focusedRow = page.locator("[role=row]:focus");
    await expect(focusedRow).toHaveCount(1);
    await page.keyboard.press("Enter");
    await expect(focusedRow).toHaveAttribute("aria-selected", "true");

    const viewport = page.getByLabel("Timeline viewport");
    const initial = await viewport.getAttribute("data-start-ns");
    const zoomIn = page.getByRole("button", { name: "Zoom timeline in" });
    await zoomIn.focus();
    await page.keyboard.press("Enter");
    await expect(zoomIn).toBeFocused();
    await expect(viewport).not.toHaveAttribute("data-start-ns", initial ?? "");
    for (const name of ["Pan timeline earlier", "Pan timeline later", "Zoom timeline out"]) {
      const control = page.getByRole("button", { name });
      await control.focus();
      await page.keyboard.press("Enter");
      await expect(control).toBeFocused();
    }
    const follow = page.getByRole("button", { name: "Follow live timeline" });
    await follow.focus();
    await page.keyboard.press("Enter");
    await expect(follow).toBeFocused();
    await expect(follow).toHaveAttribute("aria-pressed", "true");

    const eventsTab = page.getByRole("tab", { name: "Events" });
    await eventsTab.focus();
    await page.keyboard.press("Enter");
    await expect(eventsTab).toHaveAttribute("aria-selected", "true");
    const selectEvent = page.getByRole("button", { name: "Select event 7" }).first();
    await selectEvent.focus();
    await page.keyboard.press("Enter");
    await expect(selectEvent).toBeFocused();
    const inspector = page.getByLabel("Event inspector");
    await expect(inspector).toContainText("agent_message_delta");
    await expect(inspector).toContainText("message-accessible");
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
      const zoom = page.getByRole("button", { name: "Zoom timeline in" });
      await zoom.tap();
      await expect(zoom).toBeFocused();
      await page.getByRole("tab", { name: "Agent" }).tap();
      await expect(page.getByLabel("Agent transcript")).toContainText(
        "A long diagnostic message remains plain",
      );
      const geometry = await page.evaluate(() => ({
        bodyWidth: document.body.scrollWidth,
        viewportWidth: document.documentElement.clientWidth,
        clipped: [...document.querySelectorAll<HTMLElement>("button, output, [role=treeitem]")]
          .filter((element) => {
            const rect = element.getBoundingClientRect();
            return rect.right > innerWidth + 1 || rect.left < -1;
          }).length,
      }));
      expect(geometry.bodyWidth).toBeLessThanOrEqual(geometry.viewportWidth);
      expect(geometry.clipped).toBe(0);
    } finally {
      await context.close();
    }
  });
}
