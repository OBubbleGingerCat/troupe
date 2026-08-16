import { createServer, type IncomingMessage, type ServerResponse } from "node:http";
import { readFileSync } from "node:fs";
import { resolve } from "node:path";

import { expect, test, type Page, type Response } from "@playwright/test";


const FRONTEND_ROOT = resolve(import.meta.dirname, "../..");
const REPOSITORY_ROOT = resolve(FRONTEND_ROOT, "../..");
const GENERATED_ROOT = resolve(
  REPOSITORY_ROOT,
  "rust/crates/troupe-diagnostics-runtime/assets/generated",
);
const HTTP_FIXTURE_ROOT = resolve(REPOSITORY_ROOT, "tests/fixtures/diagnostics/http");
const LOOPBACK_NO_PROXY = "127.0.0.1,localhost,::1";
const RUN_ID = "12345678-1234-4234-9234-123456789abc";
const SECURITY_HEADERS = {
  "Content-Security-Policy": "default-src 'none'; script-src 'self'; style-src 'self' 'unsafe-inline'; connect-src 'self'; img-src 'self' data:; font-src 'self'; base-uri 'none'; form-action 'none'; frame-ancestors 'none'; object-src 'none'; worker-src 'self'; manifest-src 'self'",
  "X-Content-Type-Options": "nosniff",
  "Referrer-Policy": "no-referrer",
  "Cross-Origin-Resource-Policy": "same-origin",
  "Cross-Origin-Opener-Policy": "same-origin",
} as const;

process.env.NO_PROXY = LOOPBACK_NO_PROXY;
process.env.no_proxy = LOOPBACK_NO_PROXY;

interface GeneratedFile {
  readonly path: string;
  readonly url: string;
  readonly kind: "js" | "css";
  readonly encoding: "raw" | "gzip" | "br";
  readonly content_encoding: "gzip" | "br" | null;
  readonly mime: string;
  readonly cache_control: string;
  readonly sha256: string;
  readonly bytes: number;
}

interface GeneratedManifest {
  readonly schema_version: number;
  readonly build_sha256: string;
  readonly html: {
    readonly url: string;
    readonly mime: string;
    readonly cache_control: string;
    readonly sha256: string;
    readonly bytes: number;
    readonly content: string;
  };
  readonly files: readonly GeneratedFile[];
}

type Scenario = "archive" | "incompatible" | "missing-capability";

interface RequestRecord {
  readonly scenario: Scenario;
  readonly method: string;
  readonly path: string;
  readonly representation: GeneratedFile["encoding"] | null;
}

const BASE_PATHS: Readonly<Record<Scenario, string>> = {
  archive: "/proxy/archive",
  incompatible: "/proxy/incompatible",
  "missing-capability": "/proxy/missing-capability",
};

const manifest = JSON.parse(
  readFileSync(resolve(GENERATED_ROOT, "manifest.json"), "utf8"),
) as GeneratedManifest;
const statusFixture = fixture("status-v1.json");
const snapshotFixture = fixture("snapshot-v1.json");
const eventsFixture = fixture("events-v1.json");
const viewCatalogFixture = fixture("view-catalog-v1.json");
const representations = new Map<string, Map<GeneratedFile["encoding"], GeneratedFile>>();
for (const file of manifest.files) {
  const path = new URL(file.url, "http://diagnostics.test/").pathname;
  const group = representations.get(path) ?? new Map();
  group.set(file.encoding, file);
  representations.set(path, group);
}

let origin = "";
let requests: RequestRecord[] = [];
const server = createServer(handleRequest);

function fixture(name: string): Record<string, unknown> {
  return JSON.parse(readFileSync(resolve(HTTP_FIXTURE_ROOT, name), "utf8")) as Record<
    string,
    unknown
  >;
}

function scenarioForPath(pathname: string): { readonly scenario: Scenario; readonly base: string } | null {
  for (const [scenario, base] of Object.entries(BASE_PATHS) as [Scenario, string][]) {
    if (pathname === `${base}/` || pathname.startsWith(`${base}/`)) {
      return { scenario, base };
    }
  }
  return null;
}

function identity(scenario: Scenario, base: string): Record<string, unknown> {
  const incompatible = scenario === "incompatible";
  return {
    identity_schema_version: 1,
    server_protocol_version: incompatible ? 2 : 1,
    event_schema_version: incompatible ? 2 : 1,
    view_schema_version: incompatible ? 2 : 1,
    api_schema_version: incompatible ? 2 : 1,
    run_id: RUN_ID,
    owner_pid: process.pid,
    process_identity: "playwright-http-assets",
    bind_host: "0.0.0.0",
    port: Number(new URL(origin).port),
    local_endpoint: `${origin}${base}/`,
    advertise_url: `${origin}${base}/`,
    base_path: base,
    api_base_path: `${base}/api/v1`,
    identity_path: `${base}/api/v1/identity`,
    security_scope: "trusted_network",
    operational_limits: {},
  };
}

function writeHeaders(response: ServerResponse, headers: Readonly<Record<string, string>>): void {
  for (const [name, value] of Object.entries(headers)) {
    response.setHeader(name, value);
  }
}

function finish(
  request: IncomingMessage,
  response: ServerResponse,
  status: number,
  body: Buffer,
): void {
  response.statusCode = status;
  response.setHeader("Content-Length", body.byteLength.toString());
  response.end(request.method === "HEAD" ? undefined : body);
}

function accepts(request: IncomingMessage, encoding: "br" | "gzip"): boolean {
  const value = request.headers["accept-encoding"] ?? "";
  return value
    .split(",")
    .map((part) => part.trim().toLowerCase())
    .some((part) => part === encoding || part.startsWith(`${encoding};`));
}

function serveUi(
  request: IncomingMessage,
  response: ServerResponse,
  relativePath: string,
): GeneratedFile["encoding"] | null {
  writeHeaders(response, SECURITY_HEADERS);
  if (relativePath === "/") {
    const body = Buffer.from(manifest.html.content);
    const etag = `"sha256-${manifest.html.sha256}"`;
    response.setHeader("Content-Type", manifest.html.mime);
    response.setHeader("Cache-Control", manifest.html.cache_control);
    response.setHeader("ETag", etag);
    if (request.headers["if-none-match"] === etag) {
      response.statusCode = 304;
      response.end();
    } else {
      finish(request, response, 200, body);
    }
    return null;
  }

  const group = representations.get(relativePath);
  if (group === undefined) {
    response.statusCode = 404;
    response.setHeader("Cache-Control", "no-store");
    response.end();
    return null;
  }
  const encoding = accepts(request, "br") ? "br" : accepts(request, "gzip") ? "gzip" : "raw";
  const file = group.get(encoding);
  if (file === undefined) {
    throw new Error(`generated ${relativePath} is missing ${encoding}`);
  }
  const body = readFileSync(resolve(REPOSITORY_ROOT, file.path));
  const etag = `"sha256-${file.sha256}"`;
  response.setHeader("Content-Type", file.mime);
  response.setHeader("Cache-Control", file.cache_control);
  response.setHeader("Vary", "Accept-Encoding");
  response.setHeader("ETag", etag);
  if (file.content_encoding !== null) {
    response.setHeader("Content-Encoding", file.content_encoding);
  }
  if (request.headers["if-none-match"] === etag) {
    response.statusCode = 304;
    response.end();
  } else {
    finish(request, response, 200, body);
  }
  return encoding;
}

function serveJson(
  request: IncomingMessage,
  response: ServerResponse,
  value: Record<string, unknown>,
): void {
  response.setHeader("Content-Type", "application/json; charset=utf-8");
  response.setHeader("Cache-Control", "no-store");
  finish(request, response, 200, Buffer.from(JSON.stringify(value)));
}

function handleRequest(request: IncomingMessage, response: ServerResponse): void {
  try {
    const url = new URL(request.url ?? "/", "http://127.0.0.1");
    const match = scenarioForPath(url.pathname);
    if (match === null) {
      response.statusCode = 404;
      response.end();
      return;
    }
    const relativePath = url.pathname.slice(match.base.length);
    if (relativePath === "/" || representations.has(relativePath)) {
      const representation = serveUi(request, response, relativePath);
      requests.push({
        scenario: match.scenario,
        method: request.method ?? "GET",
        path: relativePath,
        representation,
      });
      return;
    }

    const apiRelative = relativePath.replace(/^\/api\/v1\//, "");
    let value: Record<string, unknown> | null = null;
    if (apiRelative === "identity") {
      value = identity(match.scenario, match.base);
    } else if (apiRelative === "status") {
      value = structuredClone(statusFixture);
      if (match.scenario === "incompatible") {
        value.event_schema_version = "2";
      }
    } else if (apiRelative === "snapshot") {
      value = structuredClone(snapshotFixture);
    } else if (apiRelative === "events" && url.searchParams.has("through")) {
      value = structuredClone(eventsFixture);
    } else if (apiRelative === "views") {
      value = structuredClone(viewCatalogFixture);
      value.views = [];
    }
    requests.push({
      scenario: match.scenario,
      method: request.method ?? "GET",
      path: relativePath,
      representation: null,
    });
    if (value === null) {
      response.statusCode = 404;
      response.setHeader("Cache-Control", "no-store");
      response.end();
      return;
    }
    serveJson(request, response, value);
  } catch (error) {
    response.statusCode = 500;
    response.end(error instanceof Error ? error.message : String(error));
  }
}

function observe(page: Page): {
  readonly consoleErrors: string[];
  readonly pageErrors: string[];
  readonly urls: string[];
  readonly responses: Response[];
} {
  const consoleErrors: string[] = [];
  const pageErrors: string[] = [];
  const urls: string[] = [];
  const responses: Response[] = [];
  page.on("console", (message) => {
    if (message.type() === "error") {
      consoleErrors.push(message.text());
    }
  });
  page.on("pageerror", (error) => pageErrors.push(error.message));
  page.on("request", (request) => urls.push(request.url()));
  page.on("response", (response) => responses.push(response));
  return { consoleErrors, pageErrors, urls, responses };
}

function apiPaths(scenario: Scenario): string[] {
  return requests
    .filter((record) => record.scenario === scenario && record.path.startsWith("/api/"))
    .map((record) => record.path);
}

function expectSameOriginClean(observed: ReturnType<typeof observe>): void {
  expect(observed.consoleErrors).toEqual([]);
  expect(observed.pageErrors).toEqual([]);
  expect(observed.urls.length).toBeGreaterThan(0);
  observed.urls.forEach((value) => expect(new URL(value).origin).toBe(origin));
}

test.beforeAll(async () => {
  await new Promise<void>((resolveListen, reject) => {
    server.once("error", reject);
    server.listen(0, "127.0.0.1", () => resolveListen());
  });
  const address = server.address();
  if (address === null || typeof address === "string") {
    throw new Error("diagnostic asset test server did not bind an inet address");
  }
  origin = `http://127.0.0.1:${address.port}`;
});

test.afterAll(async () => {
  await new Promise<void>((resolveClose, reject) => {
    server.close((error) => error === undefined ? resolveClose() : reject(error));
  });
});

test.beforeEach(() => {
  requests = [];
});

test("loads the embedded archive UI through a reverse-proxy subpath", async ({ page }) => {
  const observed = observe(page);
  const documentResponse = await page.goto(`${origin}${BASE_PATHS.archive}/`, {
    waitUntil: "networkidle",
  });
  expect(documentResponse).not.toBeNull();
  await expect(page.locator(".diagnostics-root")).toHaveAttribute("data-source", "archive");
  await expect(page.getByRole("heading", { name: "Troupe Diagnostics" })).toBeVisible();
  await expect(
    page.getByRole("banner").getByText("Archived production", { exact: true }),
  ).toBeVisible();

  const capabilities = await page.evaluate(() => ({
    fetch: typeof fetch,
    EventSource: typeof EventSource,
    URL: typeof URL,
    BigInt: typeof BigInt,
  }));
  expect(capabilities).toEqual({
    fetch: "function",
    EventSource: "function",
    URL: "function",
    BigInt: "function",
  });

  const documentHeaders = await documentResponse!.allHeaders();
  expect(documentHeaders["cache-control"]).toBe("no-cache");
  expect(documentHeaders["content-security-policy"]).toContain("script-src 'self'");
  expect(documentHeaders["content-security-policy"]).not.toContain("'unsafe-eval'");
  expect(documentHeaders["x-content-type-options"]).toBe("nosniff");
  expect(documentHeaders["referrer-policy"]).toBe("no-referrer");
  expect(documentHeaders["cross-origin-resource-policy"]).toBe("same-origin");
  expect(documentHeaders["cross-origin-opener-policy"]).toBe("same-origin");
  expect(Object.keys(documentHeaders).some((name) => name.startsWith("access-control-"))).toBe(false);

  const assetResponses = observed.responses.filter((response) => (
    response.url().startsWith(`${origin}${BASE_PATHS.archive}/assets/`)
  ));
  expect(assetResponses).toHaveLength(2);
  for (const response of assetResponses) {
    const headers = await response.allHeaders();
    expect(headers["cache-control"]).toBe("public, max-age=31536000, immutable");
    expect(headers.vary).toBe("Accept-Encoding");
    expect(headers.etag).toMatch(/^"sha256-[0-9a-f]{64}"$/);
    expect(["br", "gzip"]).toContain(headers["content-encoding"]);
    expect(headers["content-type"]).toMatch(/^(text\/javascript|text\/css); charset=utf-8$/);
  }
  expect(apiPaths("archive")).toEqual(expect.arrayContaining([
    "/api/v1/identity",
    "/api/v1/status",
    "/api/v1/snapshot",
    "/api/v1/events",
  ]));
  expectSameOriginClean(observed);
});

test("validates server schema versions before snapshot or live transport", async ({ page }) => {
  const observed = observe(page);
  await page.goto(`${origin}${BASE_PATHS.incompatible}/`, { waitUntil: "networkidle" });

  const status = page.getByRole("status", { name: "Compatibility status" });
  await expect(status).toContainText("server and interface schema versions are incompatible");
  expect(apiPaths("incompatible")).toEqual([
    "/api/v1/identity",
    "/api/v1/status",
  ]);
  expectSameOriginClean(observed);
});

test("falls back to a static surface when EventSource is unavailable", async ({ page }) => {
  await page.addInitScript(() => {
    Object.defineProperty(globalThis, "EventSource", {
      configurable: true,
      value: undefined,
    });
  });
  const observed = observe(page);
  await page.goto(`${origin}${BASE_PATHS["missing-capability"]}/`, { waitUntil: "networkidle" });

  const status = page.getByRole("status", { name: "Compatibility status" });
  await expect(status).toContainText("Required browser capabilities are unavailable");
  expect(await page.evaluate(() => typeof EventSource)).toBe("undefined");
  expect(apiPaths("missing-capability")).toEqual([
    "/api/v1/identity",
    "/api/v1/status",
  ]);
  expectSameOriginClean(observed);
});

test("serves HEAD, conditional, and representation-specific cache contracts", async ({ request }) => {
  const base = `${origin}${BASE_PATHS.archive}`;
  const assetPath = [...representations.keys()].find((path) => path.endsWith(".js"));
  expect(assetPath).toBeDefined();

  const head = await request.head(`${base}/`);
  expect(head.status()).toBe(200);
  expect((await head.body()).byteLength).toBe(0);
  expect(head.headers()["content-length"]).toBe(String(manifest.html.bytes));

  const raw = await request.get(`${base}${assetPath!}`, {
    headers: { "Accept-Encoding": "identity" },
  });
  const gzip = await request.get(`${base}${assetPath!}`, {
    headers: { "Accept-Encoding": "gzip" },
  });
  expect(raw.status()).toBe(200);
  expect(gzip.status()).toBe(200);
  expect(raw.headers().etag).not.toBe(gzip.headers().etag);
  expect(gzip.headers()["content-encoding"]).toBe("gzip");

  const notModified = await request.get(`${base}${assetPath!}`, {
    headers: {
      "Accept-Encoding": "gzip",
      "If-None-Match": gzip.headers().etag,
    },
  });
  expect(notModified.status()).toBe(304);
  expect((await notModified.body()).byteLength).toBe(0);

  const otherHtml = await request.get(`${origin}${BASE_PATHS.incompatible}/`);
  const archiveHtml = await request.get(`${base}/`);
  expect(await otherHtml.body()).toEqual(await archiveHtml.body());
});
