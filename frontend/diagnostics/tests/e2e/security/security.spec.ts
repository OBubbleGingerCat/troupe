import { spawnSync } from "node:child_process";
import { readFileSync, readdirSync } from "node:fs";
import { resolve } from "node:path";

import { expect, test, type Page, type Response } from "@playwright/test";
import { createServer, type Plugin, type ViteDevServer } from "vite";


const PROJECT_ROOT = resolve(import.meta.dirname, "../../..");
const CONTENT = JSON.parse(
  readFileSync(resolve(import.meta.dirname, "malicious-events.json"), "utf8"),
) as {
  readonly cases: readonly { readonly id: string; readonly text: string }[];
};
const NETWORK = JSON.parse(
  readFileSync(resolve(import.meta.dirname, "network-allowlist.json"), "utf8"),
) as { readonly same_origin_only: boolean; readonly path_prefixes: readonly string[] };
const RESPONSE = JSON.parse(
  readFileSync(resolve(import.meta.dirname, "response-headers.json"), "utf8"),
) as {
  readonly headers: Readonly<Record<string, string>>;
  readonly forbidden_header_prefixes: readonly string[];
};
const LOOPBACK_NO_PROXY = "127.0.0.1,localhost,::1";
process.env.NO_PROXY = LOOPBACK_NO_PROXY;
process.env.no_proxy = LOOPBACK_NO_PROXY;

const FIXTURE_TEMPLATE = String.raw`
import { h, render } from "preact";
import { App } from "/src/app.tsx";
import { decodeDiagnosticEvent } from "/src/protocol/event.ts";
import { createDiagnosticState, reduceDiagnosticState } from "/src/state/reducer.ts";

const RUN_ID = "12345678-1234-4234-9234-123456789abc";
const mode = new URL(location.href).searchParams.get("mode") || "active";
const cases = __V13_CONTENT__;
const scope = (index) => ({
  scene_id: "scene-security",
  actor_id: "actor-security",
  cue_id: "cue-" + String(index + 1),
  effect_id: null,
  act_id: "act-" + String(index + 1),
  tool_call_id: null,
  session_generation: "1",
});
const events = cases.map((item, index) => decodeDiagnosticEvent({
  kind: "agent_message_delta",
  schema_version: 1,
  run_id: RUN_ID,
  sequence: String(index + 1),
  elapsed_ns: String((index + 1) * 10),
  scope: scope(index),
  caused_by: [],
  message_id: "message-" + item.id,
  source_message_id: null,
  text_delta: item.text,
}));
let diagnostics = events.reduce(
  (state, event) => reduceDiagnosticState(state, { type: "event_received", event }),
  createDiagnosticState(RUN_ID, "0"),
);
const compatibility = mode === "compatibility";
const source = mode === "archive" ? "archive" : "active";
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
  loadCatalog: async () => ({ api_schema_version: 1, run_id: RUN_ID, capabilities: {}, views: [] }),
  query: async () => { throw new Error("empty fixture has no views"); },
  reportRendererFailure: (_id, error) => error,
  invalidateView() {}, dispose() {},
});
render(h(App, {
  liveController: new Controller(), viewClientFactory: emptyViews,
  productionName: "Security production",
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
  const source = FIXTURE_TEMPLATE.replace("__V13_CONTENT__", JSON.stringify(CONTENT.cases));
  return {
    name: "v13-security-fixture",
    resolveId(id) {
      return id === "/__v13-entry.js" ? "\0v13-entry.js" : null;
    },
    load(id) {
      return id === "\0v13-entry.js" ? source : null;
    },
    configureServer(server) {
      server.middlewares.use(async (request, response, next) => {
        const pathname = new URL(request.url ?? "/", "http://127.0.0.1").pathname;
        if (pathname !== "/__v13") {
          next();
          return;
        }
        try {
          const html = await server.transformIndexHtml(pathname, String.raw`<!doctype html>
<html lang="en"><head><meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1">
<title>Troupe security acceptance</title></head><body><main id="app"></main>
<script type="module" src="/__v13-entry.js"></script></body></html>`);
          response.statusCode = 200;
          response.setHeader("Content-Type", "text/html; charset=utf-8");
          response.setHeader("Cache-Control", "no-cache");
          for (const [name, value] of Object.entries(RESPONSE.headers)) {
            response.setHeader(name, value);
          }
          response.end(html);
        } catch (error) {
          next(error as Error);
        }
      });
    },
  };
}

function observe(page: Page): {
  readonly requests: string[];
  readonly responses: Response[];
  readonly errors: string[];
  readonly downloads: string[];
} {
  const requests: string[] = [];
  const responses: Response[] = [];
  const errors: string[] = [];
  const downloads: string[] = [];
  page.on("request", (request) => requests.push(request.url()));
  page.on("response", (response) => responses.push(response));
  page.on("pageerror", (error) => errors.push(error.message));
  page.on("console", (message) => {
    if (message.type() === "error") errors.push(message.text());
  });
  page.on("download", (download) => downloads.push(download.suggestedFilename()));
  return { requests, responses, errors, downloads };
}

function sourceFiles(root: string): string[] {
  return readdirSync(root, { withFileTypes: true }).flatMap((entry) => {
    const path = resolve(root, entry.name);
    return entry.isDirectory()
      ? sourceFiles(path)
      : /\.[cm]?[jt]sx?$/.test(entry.name) ? [path] : [];
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
    cacheDir: resolve(cacheRoot, `vite-v13-${browserName}`),
    logLevel: "error",
    server: {
      host: "127.0.0.1",
      port: 0,
      strictPort: false,
      cors: false,
      headers: RESPONSE.headers,
    },
    plugins: [fixturePlugin()],
  });
  await server.listen();
  const address = server.httpServer?.address();
  if (address === null || address === undefined || typeof address === "string") {
    throw new Error("V13 fixture did not bind an inet address");
  }
  origin = `http://127.0.0.1:${address.port}`;
});

test.afterAll(async () => {
  await server?.close();
  server = null;
});

test("active and archive content remains text under the exact response policy", async ({ page }) => {
  await page.addInitScript(() => {
    Object.defineProperty(globalThis, "__v13Executed", { configurable: true, value: false, writable: true });
  });
  for (const mode of ["active", "archive"]) {
    const observed = observe(page);
    const documentResponse = await page.goto(`${origin}/__v13?mode=${mode}`, {
      waitUntil: "networkidle",
    });
    expect(documentResponse).not.toBeNull();
    await page.getByRole("tab", { name: "Agent" }).click();
    await expect(page.getByLabel("Agent transcript")).toBeVisible();
    for (const item of CONTENT.cases) {
      await expect(page.getByText(item.text, { exact: true })).toBeVisible();
    }
    expect(await page.evaluate(() => (globalThis as { __v13Executed?: boolean }).__v13Executed))
      .toBe(false);
    await expect(page.locator(".diagnostic-transcript img, .diagnostic-transcript script"))
      .toHaveCount(0);
    await expect(page.locator(".diagnostic-transcript a, iframe, object, embed")).toHaveCount(0);
    expect(page.url()).toBe(`${origin}/__v13?mode=${mode}#/agent`);
    expect(observed.downloads).toEqual([]);
    expect(observed.errors).toEqual([]);

    const documentHeaders = await documentResponse!.allHeaders();
    for (const [name, expected] of Object.entries(RESPONSE.headers)) {
      expect(documentHeaders[name], name).toBe(expected);
    }

    expect(observed.requests.length).toBeGreaterThan(0);
    for (const value of observed.requests) {
      const url = new URL(value);
      expect(url.origin).toBe(origin);
      expect(NETWORK.path_prefixes.some((prefix) => url.pathname.startsWith(prefix)), value)
        .toBe(true);
    }
    for (const response of observed.responses) {
      const headers = await response.allHeaders();
      expect(Object.keys(headers).some((name) => (
        RESPONSE.forbidden_header_prefixes.some((prefix) => name.startsWith(prefix))
      ))).toBe(false);
    }
    const clientState = await page.evaluate(async () => ({
      local: localStorage.length,
      session: sessionStorage.length,
      serviceWorkers: "serviceWorker" in navigator
        ? (await navigator.serviceWorker.getRegistrations()).length
        : 0,
    }));
    expect(clientState).toEqual({ local: 0, session: 0, serviceWorkers: 0 });
  }
});

test("compatibility mode creates no API fetch or EventSource", async ({ page }) => {
  await page.addInitScript(() => {
    const calls = { fetch: [] as string[], eventSource: [] as string[] };
    Object.defineProperty(globalThis, "__v13Calls", { value: calls });
    const nativeFetch = globalThis.fetch.bind(globalThis);
    globalThis.fetch = (input, init) => {
      calls.fetch.push(String(input));
      return nativeFetch(input, init);
    };
    const NativeEventSource = globalThis.EventSource;
    globalThis.EventSource = class extends NativeEventSource {
      constructor(url: string | URL, options?: EventSourceInit) {
        calls.eventSource.push(String(url));
        super(url, options);
      }
    };
  });
  await page.goto(`${origin}/__v13?mode=compatibility`, { waitUntil: "networkidle" });
  await expect(page.getByRole("status", { name: "Compatibility status" })).toBeVisible();
  const calls = await page.evaluate(() => (
    globalThis as unknown as { __v13Calls: { fetch: string[]; eventSource: string[] } }
  ).__v13Calls);
  expect(calls).toEqual({ fetch: [], eventSource: [] });
});

test("frontend source contains no imperative HTML or browser persistence escape hatch", () => {
  const sources = sourceFiles(resolve(PROJECT_ROOT, "src"));
  expect(sources.length).toBeGreaterThan(20);
  for (const path of sources) {
    const source = readFileSync(path, "utf8");
    expect(source, path).not.toContain("dangerouslySetInnerHTML");
    expect(source, path).not.toMatch(/\.innerHTML\s*=/);
    expect(source, path).not.toMatch(/\b(?:localStorage|sessionStorage|indexedDB|serviceWorker)\b/);
  }
});
