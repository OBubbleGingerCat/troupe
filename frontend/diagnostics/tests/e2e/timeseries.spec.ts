import { spawnSync } from "node:child_process";
import { resolve } from "node:path";

import { expect, test } from "@playwright/test";
import {
  createServer,
  type Plugin,
  type ViteDevServer,
} from "vite";


const PROJECT_ROOT = resolve(import.meta.dirname, "../..");
const LOOPBACK_NO_PROXY = "127.0.0.1,localhost,::1";
process.env.NO_PROXY = LOOPBACK_NO_PROXY;
process.env.no_proxy = LOOPBACK_NO_PROXY;

function webkitHostLibrariesAvailable(): boolean {
  if (process.platform !== "linux") {
    return true;
  }
  const result = spawnSync("/sbin/ldconfig", ["-p"], { encoding: "utf8" });
  if (result.status !== 0) {
    return true;
  }
  return ["libgstcodecparsers-1.0.so.0", "libavif.so.13"]
    .every((library) => result.stdout.includes(library));
}

const WEBKIT_HOST_READY = webkitHostLibrariesAvailable();
const TEST_PAGE = String.raw`<!doctype html>
<html lang="en">
  <head>
    <meta charset="utf-8">
    <meta name="viewport" content="width=device-width, initial-scale=1">
    <title>W13 TimeSeries renderer</title>
    <style>
      * { box-sizing: border-box; }
      html, body { margin: 0; min-height: 100%; }
      body { font: 14px/1.4 system-ui, sans-serif; padding: 18px; }
      #app { margin: 0 auto; max-width: 960px; min-width: 0; width: 100%; }
    </style>
  </head>
  <body>
    <main id="app"></main>
    <script type="module">
      import { TimeSeriesRenderer } from "/src/timeseries/renderer.ts";

      const coverage = (contributing, gaps = "0") => ({
        status: gaps === "0" ? "complete" : "partial",
        matched_count: contributing,
        contributing_count: contributing,
        excluded_count: "0",
        excluded: {
          open_spans: "0",
          missing_values: "0",
          non_numeric_values: "0",
          unavailable_values: "0",
          resource_truncated: "0",
        },
        gap_count: gaps,
      });
      const values = [
        { aggregate: "exact", value: { type: "integer", value: "1" } },
        { aggregate: "exact", value: { type: "decimal", value: "0.5" } },
        { aggregate: "exact", value: { type: "integer", value: "9007199254740993" } },
        { aggregate: "exact", value: { type: "integer", value: "2" } },
      ];
      const model = {
        range_start_ns: "18446744073709550000",
        range_end_ns: "18446744073709550004",
        captured_watermark: "9",
        captured_elapsed_end_ns: "18446744073709550004",
        bucket_width_ns: "1",
        bucket_start_ns: [
          "18446744073709550000",
          "18446744073709550001",
          "18446744073709550002",
          "18446744073709550003",
        ],
        bucket_end_ns: [
          "18446744073709550001",
          "18446744073709550002",
          "18446744073709550003",
          "18446744073709550004",
        ],
        partial: [true, false, false, false],
        series: [{
          group: null,
          values,
          coverage: values.map(() => coverage("1")),
        }],
        coverage: {
          ...coverage("4", "1"),
          matched_count: "5",
          excluded_count: "1",
          excluded: {
            ...coverage("0").excluded,
            resource_truncated: "1",
          },
        },
        truncated: true,
      };
      const host = document.querySelector("#app");
      let theme = "light";
      let selection = null;
      let renderer = new TimeSeriesRenderer(host, {
        model,
        title: "Queue depth",
        theme,
        selection,
      });
      window.__w13 = {
        setTheme(next) {
          theme = next;
          renderer.update({ model, title: "Queue depth", theme, selection });
        },
        setSelection(start_ns, end_ns) {
          selection = { start_ns, end_ns };
          renderer.setSelection(selection);
        },
        destroy() {
          renderer.destroy();
        },
      };
    </script>
  </body>
</html>`;

let server: ViteDevServer | null = null;
let origin = "";

test.skip(
  ({ browserName }) => browserName === "webkit" && !WEBKIT_HOST_READY,
  "host lacks the shared libraries required by the pinned WebKit build",
);

function pagePlugin(): Plugin {
  return {
    name: "w13-timeseries-test-page",
    configureServer(vite) {
      vite.middlewares.use(async (request, response, next) => {
        const pathname = new URL(request.url ?? "/", "http://127.0.0.1").pathname;
        if (pathname !== "/__w13") {
          next();
          return;
        }
        try {
          const html = await vite.transformIndexHtml(pathname, TEST_PAGE);
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

test.beforeAll(async ({ browserName }) => {
  const cacheRoot = process.env.TROUPE_GATE_TMP ?? PROJECT_ROOT;
  server = await createServer({
    root: PROJECT_ROOT,
    cacheDir: resolve(cacheRoot, `vite-w13-${browserName}`),
    logLevel: "error",
    server: { host: "127.0.0.1", port: 0, strictPort: false },
    plugins: [pagePlugin()],
  });
  await server.listen();
  const address = server.httpServer?.address();
  if (address === null || address === undefined || typeof address === "string") {
    throw new Error("Vite did not publish an inet test address");
  }
  origin = `http://127.0.0.1:${address.port}`;
});

test.afterAll(async () => {
  await server?.close();
  server = null;
});

async function canvasPixels(page: import("@playwright/test").Page): Promise<{
  readonly canvases: number;
  readonly painted: number;
}> {
  return page.locator("canvas").evaluateAll((canvases) => {
    let painted = 0;
    canvases.forEach((element) => {
      const canvas = element as HTMLCanvasElement;
      const context = canvas.getContext("2d");
      if (context === null || canvas.width === 0 || canvas.height === 0) {
        return;
      }
      const pixels = context.getImageData(0, 0, canvas.width, canvas.height).data;
      for (let index = 3; index < pixels.length; index += 4) {
        if (pixels[index] !== 0) {
          painted += 1;
        }
      }
    });
    return { canvases: canvases.length, painted };
  });
}

function attachFailureCapture(page: import("@playwright/test").Page): {
  readonly consoleErrors: string[];
  readonly pageErrors: string[];
  readonly networkUrls: string[];
} {
  const consoleErrors: string[] = [];
  const pageErrors: string[] = [];
  const networkUrls: string[] = [];
  page.on("console", (message) => {
    if (message.type() === "error") {
      consoleErrors.push(message.text());
    }
  });
  page.on("pageerror", (error) => pageErrors.push(error.message));
  page.on("request", (request) => networkUrls.push(request.url()));
  page.on("websocket", (socket) => networkUrls.push(socket.url()));
  return { consoleErrors, pageErrors, networkUrls };
}

function expectLoopbackOnly(urls: readonly string[]): void {
  expect(urls.length).toBeGreaterThan(0);
  urls.forEach((value) => {
    const url = new URL(value);
    expect(["127.0.0.1", "localhost", "[::1]"]).toContain(url.hostname);
  });
}

test("renders exact coverage and a nonblank canvas on desktop", async ({ page }, testInfo) => {
  const observed = attachFailureCapture(page);
  await page.setViewportSize({ width: 1180, height: 780 });
  await page.goto(`${origin}/__w13`, { waitUntil: "networkidle" });

  const root = page.getByRole("region", { name: "Queue depth time series" });
  await expect(root).toHaveAttribute("data-coverage", "truncated");
  await expect(root).toContainText("18446744073709550000");
  await expect(root).toContainText("9007199254740993");
  await expect(root).toContainText("Text-only values1");
  await expect(root).toContainText("Observation gaps1");
  await expect(root).toContainText("Partial buckets1");
  await expect(page.getByRole("img", { name: "Queue depth plot" })).toBeVisible();

  const pixels = await canvasPixels(page);
  expect(pixels.canvases).toBeGreaterThan(0);
  expect(pixels.painted).toBeGreaterThan(100);
  const screenshot = await page.screenshot({
    path: testInfo.outputPath("timeseries-desktop.png"),
    fullPage: true,
  });
  expect(screenshot.byteLength).toBeGreaterThan(10_000);

  await page.evaluate(() => {
    const controls = (window as unknown as {
      __w13: { setSelection(start: string, end: string): void };
    }).__w13;
    controls.setSelection("18446744073709550001", "18446744073709550003");
  });
  await expect(root).toHaveAttribute("data-selection-start-ns", "18446744073709550001");
  await expect(root).toHaveAttribute("data-selection-end-ns", "18446744073709550003");

  expect(observed.consoleErrors).toEqual([]);
  expect(observed.pageErrors).toEqual([]);
  expectLoopbackOnly(observed.networkUrls);
});

test("resizes cleanly and remains contained on mobile", async ({ page }, testInfo) => {
  const observed = attachFailureCapture(page);
  await page.setViewportSize({ width: 390, height: 844 });
  await page.goto(`${origin}/__w13`, { waitUntil: "networkidle" });

  const root = page.getByRole("region", { name: "Queue depth time series" });
  const initialCanvas = await page.locator("canvas").first().boundingBox();
  expect(initialCanvas).not.toBeNull();
  expect(initialCanvas?.width).toBeLessThanOrEqual(354);
  expect(await page.evaluate(() => document.documentElement.scrollWidth <= window.innerWidth)).toBe(true);

  await page.evaluate(() => {
    (window as unknown as { __w13: { setTheme(theme: string): void } }).__w13.setTheme("dark");
  });
  await expect(root).toHaveAttribute("data-theme", "dark");
  await page.setViewportSize({ width: 340, height: 760 });
  await expect.poll(async () => (await page.locator("canvas").first().boundingBox())?.width ?? 0)
    .toBeLessThan(initialCanvas?.width ?? 0);
  expect(await page.evaluate(() => document.documentElement.scrollWidth <= window.innerWidth)).toBe(true);

  const pixels = await canvasPixels(page);
  expect(pixels.canvases).toBeGreaterThan(0);
  expect(pixels.painted).toBeGreaterThan(100);
  const screenshot = await page.screenshot({
    path: testInfo.outputPath("timeseries-mobile-dark.png"),
    fullPage: true,
  });
  expect(screenshot.byteLength).toBeGreaterThan(8_000);

  await page.evaluate(() => {
    (window as unknown as { __w13: { destroy(): void } }).__w13.destroy();
  });
  await expect(page.locator("canvas")).toHaveCount(0);
  expect(observed.consoleErrors).toEqual([]);
  expect(observed.pageErrors).toEqual([]);
  expectLoopbackOnly(observed.networkUrls);
});
