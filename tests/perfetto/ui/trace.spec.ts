import { createHash } from "node:crypto";
import { readFileSync, writeFileSync } from "node:fs";

import { expect, test, type BrowserContext, type Page } from "@playwright/test";


type Counts = {
  tracks: number;
  slices: number;
  counters: number;
  flows: number;
};

type TraceFixture = {
  name: string;
  path: string;
  sha256: string;
  counts: Counts;
  coverage?: string;
  required_labels?: string[];
  pixel_oracle?: string;
};

type FixtureManifest = {
  schema: string;
  perfetto: {
    release_tag: string;
    release_commit: string;
    ui_sha256: string;
  };
  files: TraceFixture[];
  flow_probe: TraceFixture;
};

type KeyPixel = { x: number; y: number; rgba: [number, number, number, number] };

type CanvasOracle = {
  index: number;
  width: number;
  height: number;
  minimum_opaque_pixels: number;
  minimum_distinct_colors: number;
  key_pixels: KeyPixel[];
};

type PixelFixtureOracle = {
  minimum_canvas_count: number;
  canvases: CanvasOracle[];
};

type PixelOracle = {
  schema: string;
  viewport: { width: number; height: number; device_scale_factor: number };
  timeouts_ms: { load: number; query: number; pixels: number };
  fixtures: Record<string, PixelFixtureOracle>;
};

type CanvasMeasurement = {
  index: number;
  width: number;
  height: number;
  context: "2d" | "other";
  opaquePixels: number;
  distinctColors: number;
  keyPixels: Array<{ x: number; y: number; rgba: number[] }>;
};

type PageDiagnostics = {
  consoleErrors: string[];
  pageErrors: string[];
  localFailures: string[];
};


const COUNT_HEADERS = ["track_count", "slice_count", "counter_count", "flow_count"];
const COUNT_QUERY = `select
  (select count(*) from track) as track_count,
  (select count(*) from slice) as slice_count,
  (select count(*) from counter) as counter_count,
  (select count(*) from flow) as flow_count;`;


function requiredEnvironment(name: string): string {
  const value = process.env[name];
  if (value === undefined || value.length === 0) {
    throw new Error(`missing required environment: ${name}`);
  }
  return value;
}


function loadJson<T>(name: string): T {
  return JSON.parse(readFileSync(requiredEnvironment(name), "utf8")) as T;
}


const manifest = loadJson<FixtureManifest>("TROUPE_PERFETTO_UI_MANIFEST");
const pixelOracle = loadJson<PixelOracle>("TROUPE_PERFETTO_UI_PIXEL_ORACLE");
const origin = requiredEnvironment("TROUPE_PERFETTO_UI_ORIGIN");


function attachDiagnostics(page: Page): PageDiagnostics {
  const diagnostics: PageDiagnostics = {
    consoleErrors: [],
    pageErrors: [],
    localFailures: [],
  };
  page.on("console", (message) => {
    if (message.type() === "error") {
      diagnostics.consoleErrors.push(message.text());
    }
  });
  page.on("pageerror", (error) => diagnostics.pageErrors.push(error.message));
  page.on("requestfailed", (request) => {
    if (new URL(request.url()).origin === origin) {
      diagnostics.localFailures.push(
        `${request.method()} ${request.url()}: ${request.failure()?.errorText ?? "unknown"}`,
      );
    }
  });
  page.on("response", (response) => {
    if (new URL(response.url()).origin === origin && response.status() >= 400) {
      diagnostics.localFailures.push(`${response.status()} ${response.url()}`);
    }
  });
  return diagnostics;
}


function assertNoPageErrors(label: string, diagnostics: PageDiagnostics): void {
  const failures = [
    ...diagnostics.consoleErrors.map((message) => `console: ${message}`),
    ...diagnostics.pageErrors.map((message) => `page: ${message}`),
    ...diagnostics.localFailures.map((message) => `local request: ${message}`),
  ];
  if (failures.length !== 0) {
    throw new Error(`${label} browser errors: ${failures.join(" | ")}`);
  }
}


async function waitForTraceLoaded(page: Page, fixtureName: string, timeout: number): Promise<void> {
  try {
    await page.waitForFunction(
      (expectedFixture) => {
        const input = document.querySelector(".pf-omnibox input");
        return document.title.startsWith(`${expectedFixture}.pftrace`)
          && input instanceof HTMLInputElement
          && !input.disabled
          && !input.readOnly;
      },
      fixtureName,
      { timeout },
    );
  } catch {
    throw new Error(`trace ${fixtureName} load timeout after ${timeout}ms`);
  }
}


async function measureCanvases(page: Page, oracle: PixelFixtureOracle): Promise<CanvasMeasurement[]> {
  const points = Object.fromEntries(
    oracle.canvases.map((canvas) => [String(canvas.index), canvas.key_pixels]),
  );
  return page.evaluate((requestedPoints) => (
    [...document.querySelectorAll("canvas")].map((canvas, index) => {
      const context = canvas.getContext("2d");
      if (context === null) {
        return {
          index,
          width: canvas.width,
          height: canvas.height,
          context: "other" as const,
          opaquePixels: 0,
          distinctColors: 0,
          keyPixels: [],
        };
      }
      const pixels = context.getImageData(0, 0, canvas.width, canvas.height).data;
      let opaquePixels = 0;
      const colors = new Set<string>();
      for (let offset = 0; offset < pixels.length; offset += 4) {
        if (pixels[offset + 3] !== 0) {
          opaquePixels += 1;
        }
        if (colors.size < 4096) {
          colors.add(
            `${pixels[offset]},${pixels[offset + 1]},${pixels[offset + 2]},${pixels[offset + 3]}`,
          );
        }
      }
      const keyPixels = (requestedPoints[String(index)] ?? []).map(({ x, y }) => {
        const offset = (y * canvas.width + x) * 4;
        return { x, y, rgba: [...pixels.slice(offset, offset + 4)] };
      });
      return {
        index,
        width: canvas.width,
        height: canvas.height,
        context: "2d" as const,
        opaquePixels,
        distinctColors: colors.size,
        keyPixels,
      };
    })
  ), points);
}


function assertCanvasMetrics(
  label: string,
  oracle: PixelFixtureOracle,
  measurements: CanvasMeasurement[],
): void {
  if (measurements.length < oracle.minimum_canvas_count) {
    throw new Error(
      `${label} blank canvas: expected at least ${oracle.minimum_canvas_count}, got ${measurements.length}`,
    );
  }
  for (const expected of oracle.canvases) {
    const actual = measurements[expected.index];
    if (actual === undefined || actual.context !== "2d") {
      throw new Error(`${label} blank canvas: missing 2d canvas ${expected.index}`);
    }
    if (actual.width !== expected.width || actual.height !== expected.height) {
      throw new Error(
        `${label} canvas ${expected.index} size drift: `
        + `${actual.width}x${actual.height} != ${expected.width}x${expected.height}`,
      );
    }
    if (
      actual.opaquePixels < expected.minimum_opaque_pixels
      || actual.distinctColors < expected.minimum_distinct_colors
    ) {
      throw new Error(
        `${label} blank canvas ${expected.index}: `
        + `${actual.opaquePixels} opaque/${actual.distinctColors} colors`,
      );
    }
    for (const keyPixel of expected.key_pixels) {
      const actualPixel = actual.keyPixels.find(
        (candidate) => candidate.x === keyPixel.x && candidate.y === keyPixel.y,
      );
      if (actualPixel === undefined || JSON.stringify(actualPixel.rgba) !== JSON.stringify(keyPixel.rgba)) {
        throw new Error(
          `${label} key pixel drift at canvas ${expected.index} `
          + `(${keyPixel.x},${keyPixel.y})`,
        );
      }
    }
  }
}


async function assertRequiredLabels(page: Page, fixture: TraceFixture): Promise<string[]> {
  const required = fixture.required_labels ?? [];
  if (required.length === 0) {
    return [];
  }
  await expect.poll(
    async () => await page.locator(".pf-track__title").allTextContents(),
    { timeout: pixelOracle.timeouts_ms.pixels, message: `${fixture.name} track labels` },
  ).toEqual(expect.arrayContaining(required));
  return required;
}


async function queryCounts(page: Page, fixture: TraceFixture): Promise<Counts> {
  await page.locator('a[href="#!/query"]').click();
  await page.locator('.cm-content[role="textbox"]').fill(COUNT_QUERY);
  await page.getByRole("button", { name: "Run Query" }).click();

  const expectedCells = [
    fixture.counts.tracks,
    fixture.counts.slices,
    fixture.counts.counters,
    fixture.counts.flows,
  ].map(String);
  const expectedResult = JSON.stringify({ headers: COUNT_HEADERS, cells: expectedCells });
  await expect.poll(async () => {
    const headers = (
      await page.locator('[role="columnheader"] .pf-grid-header-cell__title-wrapper').allTextContents()
    ).map((value) => value.trim()).slice(-4);
    const cells = (
      await page.locator('[role="cell"] .pf-grid-cell__content').allTextContents()
    ).map((value) => value.trim()).slice(-4);
    return JSON.stringify({ headers, cells });
  }, {
    timeout: pixelOracle.timeouts_ms.query,
    message: `${fixture.name} SQL count query`,
  }).toBe(expectedResult);
  return fixture.counts;
}


test.describe.configure({ mode: "serial" });


test("failure detectors reject blank canvas, console error, and load timeout", async ({ browser }) => {
  const context = await browser.newContext({
    viewport: {
      width: pixelOracle.viewport.width,
      height: pixelOracle.viewport.height,
    },
    deviceScaleFactor: pixelOracle.viewport.device_scale_factor,
    serviceWorkers: "block",
  });
  try {
    const blankPage = await context.newPage();
    await blankPage.setContent('<canvas width="64" height="32"></canvas>');
    const blankOracle: PixelFixtureOracle = {
      minimum_canvas_count: 1,
      canvases: [{
        index: 0,
        width: 64,
        height: 32,
        minimum_opaque_pixels: 1,
        minimum_distinct_colors: 2,
        key_pixels: [],
      }],
    };
    const blankMeasurements = await measureCanvases(blankPage, blankOracle);
    expect(() => assertCanvasMetrics("synthetic", blankOracle, blankMeasurements)).toThrow(
      /blank canvas/,
    );
    await blankPage.close();

    const errorPage = await context.newPage();
    const diagnostics = attachDiagnostics(errorPage);
    await errorPage.setContent("<main>console detector</main>");
    await errorPage.evaluate(() => console.error("synthetic console failure"));
    await expect.poll(() => diagnostics.consoleErrors.length).toBe(1);
    expect(() => assertNoPageErrors("synthetic", diagnostics)).toThrow(/console/);
    await errorPage.close();

    const timeoutPage = await context.newPage();
    await timeoutPage.setContent('<div class="pf-omnibox"><input></div>');
    await expect(waitForTraceLoaded(timeoutPage, "never-loaded", 75)).rejects.toThrow(
      /load timeout/,
    );
    await timeoutPage.close();
  } finally {
    await context.close();
  }
});


test("official v57.2 UI renders and queries every frozen trace", async ({ browser }, testInfo) => {
  expect(manifest.schema).toBe("troupe.perfetto.ui-fixtures.v1");
  expect(pixelOracle.schema).toBe("troupe.perfetto.ui-pixel-oracle.v1");

  const continuedRequests: string[] = [];
  const syntheticLoopbackRequests: string[] = [];
  const blockedPublicOrigins = new Set<string>();
  const publicUploads: string[] = [];
  const context: BrowserContext = await browser.newContext({
    viewport: {
      width: pixelOracle.viewport.width,
      height: pixelOracle.viewport.height,
    },
    deviceScaleFactor: pixelOracle.viewport.device_scale_factor,
    serviceWorkers: "block",
  });
  await context.route("**/*", async (route) => {
    const request = route.request();
    const url = new URL(request.url());
    const body = request.postDataBuffer();
    if (url.origin === origin) {
      continuedRequests.push(request.url());
      await route.continue();
      return;
    }
    const loopback = url.hostname === "127.0.0.1" || url.hostname === "[::1]";
    if (loopback) {
      syntheticLoopbackRequests.push(request.url());
      await route.fulfill({
        status: 204,
        headers: {
          "Access-Control-Allow-Credentials": "true",
          "Access-Control-Allow-Origin": origin,
        },
        body: "",
      });
      return;
    }

    blockedPublicOrigins.add(url.origin);
    if (!["GET", "HEAD"].includes(request.method()) || (body !== null && body.length !== 0)) {
      publicUploads.push(`${request.method()} ${request.url()}`);
    }
    const script = request.resourceType() === "script";
    await route.fulfill({
      status: 200,
      headers: {
        "Access-Control-Allow-Credentials": "true",
        "Access-Control-Allow-Origin": origin,
        "Cross-Origin-Resource-Policy": "cross-origin",
      },
      contentType: script ? "text/javascript" : "application/json",
      body: script ? "" : "{}",
    });
  });

  const fixtureResults: Array<Record<string, unknown>> = [];
  let screenshotSha256: string | null = null;
  try {
    const fixtures = [...manifest.files, manifest.flow_probe];
    for (const fixture of fixtures) {
      const page = await context.newPage();
      const diagnostics = attachDiagnostics(page);
      try {
        const traceUrl = `${origin}/traces/${fixture.name}.pftrace`;
        await page.goto(`${origin}/#!/?url=${encodeURIComponent(traceUrl)}`, {
          waitUntil: "commit",
          timeout: pixelOracle.timeouts_ms.load,
        });
        await waitForTraceLoaded(page, fixture.name, pixelOracle.timeouts_ms.load);
        expect(await page.title()).toContain(`${fixture.name}.pftrace`);

        const requiredLabels = await assertRequiredLabels(page, fixture);
        let canvasMeasurements: CanvasMeasurement[] = [];
        if (fixture.pixel_oracle !== undefined) {
          const fixtureOracle = pixelOracle.fixtures[fixture.pixel_oracle];
          expect(fixtureOracle, `${fixture.name} pixel oracle`).toBeDefined();
          await expect.poll(async () => {
            try {
              const measurements = await measureCanvases(page, fixtureOracle);
              assertCanvasMetrics(fixture.name, fixtureOracle, measurements);
              return true;
            } catch {
              return false;
            }
          }, {
            timeout: pixelOracle.timeouts_ms.pixels,
            message: `${fixture.name} canvas rendering`,
          }).toBe(true);
          canvasMeasurements = await measureCanvases(page, fixtureOracle);
          assertCanvasMetrics(fixture.name, fixtureOracle, canvasMeasurements);
          const screenshot = await page.screenshot({
            path: testInfo.outputPath(`${fixture.name}.png`),
            fullPage: true,
          });
          screenshotSha256 = createHash("sha256").update(screenshot).digest("hex");
        }

        const counts = await queryCounts(page, fixture);
        assertNoPageErrors(fixture.name, diagnostics);
        fixtureResults.push({
          name: fixture.name,
          sha256: fixture.sha256,
          counts,
          required_labels: requiredLabels,
          canvases: canvasMeasurements,
        });
      } finally {
        await page.close();
      }
    }
  } finally {
    await context.close();
  }

  expect(continuedRequests.length).toBeGreaterThan(0);
  expect(continuedRequests.every((url) => new URL(url).origin === origin)).toBe(true);
  expect(blockedPublicOrigins.size).toBeGreaterThan(0);
  expect(publicUploads).toEqual([]);
  expect(screenshotSha256).toMatch(/^[0-9a-f]{64}$/);

  const report = {
    schema: "troupe.perfetto.ui-report.v1",
    perfetto: manifest.perfetto,
    browser: {
      name: "chromium",
      version: requiredEnvironment("TROUPE_PERFETTO_UI_BROWSER_VERSION"),
      executable_sha256: requiredEnvironment("TROUPE_PERFETTO_UI_BROWSER_SHA256"),
    },
    fixtures: fixtureResults,
    network: {
      continued_transport: "loopback-only",
      blocked_public_origins: [...blockedPublicOrigins].sort(),
      synthetic_loopback_requests: syntheticLoopbackRequests.length,
      public_uploads: publicUploads.length,
    },
    pixels: {
      screenshot_sha256: screenshotSha256,
      oracle: manifest.flow_probe.pixel_oracle,
    },
    failure_detectors: ["blank_canvas", "console_error", "load_timeout"],
  };
  writeFileSync(
    requiredEnvironment("TROUPE_PERFETTO_UI_RESULT"),
    `${JSON.stringify(report)}\n`,
    { encoding: "utf8", flag: "wx" },
  );
});
