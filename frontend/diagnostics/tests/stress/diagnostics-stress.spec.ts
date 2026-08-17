import { execFileSync } from "node:child_process";
import { createHash } from "node:crypto";
import {
  existsSync,
  readFileSync,
  renameSync,
  writeFileSync,
} from "node:fs";
import os from "node:os";
import { dirname, isAbsolute, resolve } from "node:path";

import { expect, test, type Browser, type CDPSession, type Page } from "@playwright/test";
import { createServer, type Plugin, type ViteDevServer } from "vite";

import {
  expectedStressInvariants,
  STRESS_WORKLOAD,
  type StressBrowserResult,
  type StressDurations,
  type StressInvariants,
} from "./fixtures.ts";


const PROJECT_ROOT = resolve(import.meta.dirname, "../..");
const REPOSITORY_ROOT = resolve(PROJECT_ROOT, "../..");
const RAW_SCHEMA_PATH = resolve(import.meta.dirname, "performance-raw.schema.json");
const DURATION_KEYS = [
  "state_reduce_ms",
  "pause_reduce_ms",
  "timeline_layout_ms",
  "timeline_draw_ms",
  "hit_test_ms",
  "raf_updates_ms",
] as const satisfies readonly (keyof StressDurations)[];
const LOOPBACK_NO_PROXY = "127.0.0.1,localhost,::1";
process.env.NO_PROXY = LOOPBACK_NO_PROXY;
process.env.no_proxy = LOOPBACK_NO_PROXY;

interface JsonSchema {
  readonly type?: string | readonly string[];
  readonly const?: unknown;
  readonly enum?: readonly unknown[];
  readonly pattern?: string;
  readonly minimum?: number;
  readonly maximum?: number;
  readonly minItems?: number;
  readonly maxItems?: number;
  readonly required?: readonly string[];
  readonly additionalProperties?: boolean;
  readonly properties?: Readonly<Record<string, JsonSchema>>;
  readonly items?: JsonSchema;
}

interface PerformanceBaseline {
  readonly schema: "troupe.diagnostics.performance-baseline.v1";
  readonly repeat: number;
  readonly workload: typeof STRESS_WORKLOAD;
  readonly limits: {
    readonly duration_ms: Readonly<Record<keyof StressDurations, number>>;
    readonly duration_variance_floor_ms: number;
    readonly heap_peak_bytes: number;
    readonly heap_retained_bytes: number;
    readonly heap_recovery_ratio: number;
    readonly run_variance_ratio: number;
  };
}

interface HeapSample {
  readonly before: number;
  readonly peak: number;
  readonly after: number;
  readonly retained: number;
  readonly recovery_ratio: number;
}

interface RawSample {
  readonly attempt: number;
  readonly measured_at_epoch_ms: number;
  readonly browser_version: string;
  readonly durations_ms: StressDurations;
  readonly heap_bytes: HeapSample;
  readonly invariants: StressInvariants;
}

interface MetricSummary {
  readonly min: number;
  readonly max: number;
  readonly median: number;
  readonly variance_ratio: number;
}

function requiredEnvironment(name: string): string {
  const value = process.env[name];
  if (value === undefined || value.length === 0) {
    throw new Error(`${name} is required by the V05 stress harness`);
  }
  return value;
}

function positiveInteger(name: string): number {
  const value = requiredEnvironment(name);
  if (!/^[1-9][0-9]*$/.test(value)) {
    throw new Error(`${name} must be a positive integer`);
  }
  return Number(value);
}

const REPEAT = positiveInteger("TROUPE_STRESS_REPEAT");
const REPORT_KIND = requiredEnvironment("TROUPE_STRESS_REPORT_KIND");
if (REPORT_KIND !== "calibration" && REPORT_KIND !== "gate") {
  throw new Error("TROUPE_STRESS_REPORT_KIND must be calibration or gate");
}
const REPORT_INPUT = requiredEnvironment("TROUPE_STRESS_RAW_REPORT");
if (!isAbsolute(REPORT_INPUT)) {
  throw new Error("TROUPE_STRESS_RAW_REPORT must be absolute");
}
const REPORT_PATH = resolve(REPORT_INPUT);
const BASELINE_PATH = resolve(requiredEnvironment("TROUPE_STRESS_BASELINE"));
const BASELINE_RAW_PATH = resolve(requiredEnvironment("TROUPE_STRESS_BASELINE_RAW"));
const REVIEW_PATH = resolve(requiredEnvironment("TROUPE_STRESS_REVIEW"));
const RAW_SCHEMA = JSON.parse(readFileSync(RAW_SCHEMA_PATH, "utf8")) as JsonSchema;
const BASELINE = JSON.parse(readFileSync(BASELINE_PATH, "utf8")) as PerformanceBaseline;
const SAMPLES: RawSample[] = [];

function sha256(bytes: string | Buffer): string {
  return createHash("sha256").update(bytes).digest("hex");
}

function fileSha256(path: string): string {
  return sha256(readFileSync(path));
}

function stableJson(value: unknown): string {
  if (value === null || typeof value !== "object") {
    return JSON.stringify(value);
  }
  if (Array.isArray(value)) {
    return `[${value.map(stableJson).join(",")}]`;
  }
  const record = value as Record<string, unknown>;
  return `{${Object.keys(record).sort().map((key) => (
    `${JSON.stringify(key)}:${stableJson(record[key])}`
  )).join(",")}}`;
}

function valueType(value: unknown): string {
  if (value === null) {
    return "null";
  }
  if (Array.isArray(value)) {
    return "array";
  }
  if (typeof value === "number" && Number.isInteger(value)) {
    return "integer";
  }
  return typeof value;
}

function validateSchema(value: unknown, schema: JsonSchema, path = "report"): void {
  const allowedTypes = schema.type === undefined
    ? null
    : typeof schema.type === "string" ? [schema.type] : schema.type;
  const actualType = valueType(value);
  if (
    allowedTypes !== null
    && !allowedTypes.includes(actualType)
    && !(actualType === "integer" && allowedTypes.includes("number"))
  ) {
    throw new Error(`${path} has type ${actualType}, expected ${allowedTypes.join("|")}`);
  }
  if (schema.const !== undefined && value !== schema.const) {
    throw new Error(`${path} differs from its schema const`);
  }
  if (schema.enum !== undefined && !schema.enum.includes(value)) {
    throw new Error(`${path} is outside its schema enum`);
  }
  if (typeof value === "string" && schema.pattern !== undefined) {
    if (!new RegExp(schema.pattern).test(value)) {
      throw new Error(`${path} does not match its schema pattern`);
    }
  }
  if (typeof value === "number") {
    if (!Number.isFinite(value)) {
      throw new Error(`${path} is not finite`);
    }
    if (schema.minimum !== undefined && value < schema.minimum) {
      throw new Error(`${path} is below its schema minimum`);
    }
    if (schema.maximum !== undefined && value > schema.maximum) {
      throw new Error(`${path} exceeds its schema maximum`);
    }
  }
  if (Array.isArray(value)) {
    if (schema.minItems !== undefined && value.length < schema.minItems) {
      throw new Error(`${path} has too few items`);
    }
    if (schema.maxItems !== undefined && value.length > schema.maxItems) {
      throw new Error(`${path} has too many items`);
    }
    if (schema.items !== undefined) {
      value.forEach((item, index) => validateSchema(item, schema.items!, `${path}[${index}]`));
    }
  }
  if (value !== null && typeof value === "object" && !Array.isArray(value)) {
    const record = value as Record<string, unknown>;
    const required = schema.required ?? [];
    for (const key of required) {
      if (!Object.prototype.hasOwnProperty.call(record, key)) {
        throw new Error(`${path}.${key} is required`);
      }
    }
    if (schema.additionalProperties === false) {
      const known = new Set(Object.keys(schema.properties ?? {}));
      const extra = Object.keys(record).filter((key) => !known.has(key));
      if (extra.length > 0) {
        throw new Error(`${path} has unknown fields: ${extra.join(",")}`);
      }
    }
    for (const [key, childSchema] of Object.entries(schema.properties ?? {})) {
      if (Object.prototype.hasOwnProperty.call(record, key)) {
        validateSchema(record[key], childSchema, `${path}.${key}`);
      }
    }
  }
}

function validateBaseline(): void {
  const expectedKeys = ["schema", "repeat", "workload", "limits"];
  expect(Object.keys(BASELINE).sort()).toEqual(expectedKeys.sort());
  expect(BASELINE.schema).toBe("troupe.diagnostics.performance-baseline.v1");
  expect(BASELINE.repeat).toBe(REPEAT);
  expect(BASELINE.workload).toEqual(STRESS_WORKLOAD);
  expect(Object.keys(BASELINE.limits).sort()).toEqual([
    "duration_ms",
    "duration_variance_floor_ms",
    "heap_peak_bytes",
    "heap_recovery_ratio",
    "heap_retained_bytes",
    "run_variance_ratio",
  ].sort());
  expect(Object.keys(BASELINE.limits.duration_ms).sort()).toEqual([...DURATION_KEYS].sort());
  for (const value of Object.values(BASELINE.limits.duration_ms)) {
    expect(Number.isFinite(value) && value > 0).toBe(true);
  }
  expect(BASELINE.limits.duration_variance_floor_ms).toBeGreaterThan(0);
  expect(BASELINE.limits.heap_peak_bytes).toBeGreaterThan(0);
  expect(BASELINE.limits.heap_retained_bytes).toBeGreaterThan(0);
  expect(BASELINE.limits.heap_recovery_ratio).toBeGreaterThan(0);
  expect(BASELINE.limits.heap_recovery_ratio).toBeLessThan(1);
  expect(BASELINE.limits.run_variance_ratio).toBeGreaterThan(0);
  expect(BASELINE.limits.run_variance_ratio).toBeLessThan(1);
  if (REPORT_KIND === "gate") {
    validateSchema(JSON.parse(readFileSync(BASELINE_RAW_PATH, "utf8")), RAW_SCHEMA, "baseline_raw");
    if (!existsSync(REVIEW_PATH)) {
      throw new Error("checked-in baseline review is missing");
    }
  }
}

function fixturePlugin(): Plugin {
  return {
    name: "v05-stress-fixture",
    configureServer(server) {
      server.middlewares.use(async (request, response, next) => {
        const pathname = new URL(request.url ?? "/", "http://127.0.0.1").pathname;
        if (pathname !== "/__v05") {
          next();
          return;
        }
        try {
          const html = await server.transformIndexHtml(pathname, String.raw`<!doctype html>
<html lang="en"><head><meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1">
<title>Troupe diagnostics stress</title></head><body><div id="app"></div>
<script type="module">import { installStressFixture } from "/tests/stress/fixtures.ts"; installStressFixture();</script>
</body></html>`);
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

async function heapBytes(session: CDPSession): Promise<number> {
  await session.send("HeapProfiler.collectGarbage");
  const response = await session.send("Performance.getMetrics") as {
    readonly metrics: readonly { readonly name: string; readonly value: number }[];
  };
  const metric = response.metrics.find((candidate) => candidate.name === "JSHeapUsedSize");
  if (metric === undefined || !Number.isFinite(metric.value) || metric.value < 0) {
    throw new Error("Chromium did not expose a finite JSHeapUsedSize metric");
  }
  return Math.round(metric.value);
}

function assertInvariants(actual: StressInvariants): void {
  const expected = expectedStressInvariants();
  expect(actual.live_events).toBe(expected.live_events);
  expect(actual.span_items).toBeLessThanOrEqual(expected.span_items_max as number);
  expect(actual.message_items).toBeLessThanOrEqual(expected.message_items_max as number);
  expect(actual.context_usage_items).toBeLessThanOrEqual(expected.context_usage_items_max as number);
  expect(actual.act_usage_items).toBeLessThanOrEqual(expected.act_usage_items_max as number);
  expect(actual.tool_items).toBeLessThanOrEqual(expected.tool_items_max as number);
  expect(actual.result_items).toBeLessThanOrEqual(expected.result_items_max as number);
  expect(actual.gap_items).toBeLessThanOrEqual(expected.gap_items_max as number);
  expect(actual.query_entries).toBe(expected.query_entries);
  expect(actual.adjacent_windows).toBe(expected.adjacent_windows);
  expect(actual.pause_unseen_count).toBe(expected.pause_unseen_count);
  expect(actual.visible_primitives).toBe(expected.visible_primitives);
  expect(actual.drawn_primitives).toBe(expected.drawn_primitives);
  expect(actual.hit_examined_max).toBe(expected.hit_examined_max);
  expect(actual.raf_callbacks_pending).toBeLessThanOrEqual(
    expected.raf_callbacks_pending_max as number,
  );
  expect(actual.raf_draws).toBe(expected.raf_draws);
  expect(actual.selection_preserved).toBe(true);
  expect(actual.span_pair_complete).toBe(true);
  expect(actual.usage_coverage_complete).toBe(true);
  expect(actual.gap_state_visible).toBe(true);
  expect(actual.pause_frozen).toBe(true);
  expect(actual.resume_request_kind).toBe("server_range");
  expect(actual.resume_query_consumed).toBe(true);
  expect(actual.raw_backlog_events).toBe(expected.live_events);
  expect(actual.canvas_nonblank).toBe(true);
}

function summarize(values: readonly number[]): MetricSummary {
  const ordered = [...values].sort((left, right) => left - right);
  const middle = Math.floor(ordered.length / 2);
  const median = ordered.length % 2 === 0
    ? (ordered[middle - 1]! + ordered[middle]!) / 2
    : ordered[middle]!;
  const minimum = ordered[0]!;
  const maximum = ordered[ordered.length - 1]!;
  return {
    min: minimum,
    max: maximum,
    median,
    variance_ratio: (maximum - minimum) / Math.max(
      median,
      BASELINE.limits.duration_variance_floor_ms,
    ),
  };
}

function readText(path: string): string {
  try {
    return readFileSync(path, "utf8").trim();
  } catch {
    return "unavailable";
  }
}

function reportEnvironment(browserVersion: string) {
  const npmCache = requiredEnvironment("TROUPE_NPM_CACHE");
  const browserCache = requiredEnvironment("TROUPE_PLAYWRIGHT_CACHE");
  const packageJson = JSON.parse(readFileSync(resolve(PROJECT_ROOT, "package.json"), "utf8")) as {
    readonly devDependencies: Readonly<Record<string, string>>;
  };
  const browserManifest = JSON.parse(
    readFileSync(resolve(PROJECT_ROOT, "tests/tooling/playwright-browsers.json"), "utf8"),
  ) as {
    readonly platforms: {
      readonly "linux-x64": {
        readonly playwrightPlatform: string;
        readonly archives: readonly {
          readonly name: string;
          readonly revision: string;
          readonly browserVersion: string | null;
        }[];
      };
    };
  };
  const chromium = browserManifest.platforms["linux-x64"].archives.find(
    (entry) => entry.name === "chromium",
  );
  if (chromium === undefined || chromium.browserVersion === null) {
    throw new Error("pinned Chromium identity is missing");
  }
  const cpus = os.cpus();
  return {
    system: {
      platform: os.platform(),
      release: os.release(),
      architecture: os.arch(),
      cpu_model: cpus[0]?.model ?? "unavailable",
      logical_cpu_count: cpus.length,
      total_memory_bytes: os.totalmem(),
    },
    frequency_policy: {
      governor: readText("/sys/devices/system/cpu/cpu0/cpufreq/scaling_governor"),
      driver: readText("/sys/devices/system/cpu/cpu0/cpufreq/scaling_driver"),
      minimum_khz: readText("/sys/devices/system/cpu/cpu0/cpufreq/cpuinfo_min_freq"),
      maximum_khz: readText("/sys/devices/system/cpu/cpu0/cpufreq/cpuinfo_max_freq"),
      boost: readText("/sys/devices/system/cpu/cpufreq/boost"),
    },
    toolchain: {
      node_version: process.version,
      npm_version: execFileSync("npm", ["--version"], { encoding: "utf8" }).trim(),
      playwright_version: packageJson.devDependencies["@playwright/test"] ?? "unavailable",
      chromium_revision: chromium.revision,
      chromium_version: browserVersion,
      chromium_expected_version: chromium.browserVersion,
      playwright_platform: browserManifest.platforms["linux-x64"].playwrightPlatform,
    },
    cache: {
      package_lock_sha256: fileSha256(resolve(PROJECT_ROOT, "package-lock.json")),
      npm_cache_manifest_sha256: fileSha256(resolve(npmCache, ".troupe-npm-cache.json")),
      browser_contract_sha256: fileSha256(
        resolve(PROJECT_ROOT, "tests/tooling/playwright-browsers.json"),
      ),
      browser_cache_manifest_sha256: fileSha256(
        resolve(browserCache, ".troupe-playwright-cache.json"),
      ),
    },
  };
}

function buildReport(browserVersion: string) {
  const durations = Object.fromEntries(DURATION_KEYS.map((key) => [
    key,
    summarize(SAMPLES.map((sample) => sample.durations_ms[key])),
  ]));
  const heapSummary = {
    peak_max: Math.max(...SAMPLES.map((sample) => sample.heap_bytes.peak)),
    retained_max: Math.max(...SAMPLES.map((sample) => sample.heap_bytes.retained)),
    recovery_ratio_max: Math.max(...SAMPLES.map((sample) => sample.heap_bytes.recovery_ratio)),
  };
  const violations: string[] = [];
  for (const key of DURATION_KEYS) {
    const summary = durations[key] as MetricSummary;
    if (summary.max > BASELINE.limits.duration_ms[key]) {
      violations.push(`${key}.max=${summary.max} exceeds ${BASELINE.limits.duration_ms[key]}`);
    }
    if (summary.variance_ratio > BASELINE.limits.run_variance_ratio) {
      violations.push(
        `${key}.variance_ratio=${summary.variance_ratio} exceeds ${BASELINE.limits.run_variance_ratio}`,
      );
    }
  }
  if (heapSummary.peak_max > BASELINE.limits.heap_peak_bytes) {
    violations.push("heap peak exceeds its frozen threshold");
  }
  if (heapSummary.retained_max > BASELINE.limits.heap_retained_bytes) {
    violations.push("heap retention exceeds its frozen threshold");
  }
  if (heapSummary.recovery_ratio_max > BASELINE.limits.heap_recovery_ratio) {
    violations.push("heap recovery ratio exceeds its frozen threshold");
  }
  const status = violations.length === 0 ? "passed" : "failed";
  const summary = { duration_ms: durations, heap_bytes: heapSummary };
  const resultSha256 = sha256(stableJson({ samples: SAMPLES, summary, status, violations }));
  const currentIntegration = execFileSync("git", ["rev-parse", "HEAD"], {
    cwd: REPOSITORY_ROOT,
    encoding: "utf8",
  }).trim();
  const reference = REPORT_KIND === "gate" ? {
    baseline_sha256: fileSha256(BASELINE_PATH),
    baseline_raw_sha256: fileSha256(BASELINE_RAW_PATH),
    review_sha256: fileSha256(REVIEW_PATH),
  } : {
    baseline_sha256: fileSha256(BASELINE_PATH),
    baseline_raw_sha256: null,
    review_sha256: null,
  };
  return {
    schema: "troupe.diagnostics.performance-raw.v1",
    kind: REPORT_KIND,
    identity: {
      actor_design_sha256: fileSha256(resolve(REPOSITORY_ROOT, "docs/design/actor-agent-session.md")),
      diagnostics_design_sha256: fileSha256(
        resolve(REPOSITORY_ROOT, "docs/design/production-diagnostics.md"),
      ),
      plan_sha256: fileSha256(
        resolve(REPOSITORY_ROOT, "docs/plan/production-diagnostics-implementation-plan.md"),
      ),
      validator_sha256: fileSha256(
        resolve(REPOSITORY_ROOT, "docs/plan/verify_production_diagnostics_plan.py"),
      ),
      review_record_sha256: fileSha256(
        resolve(REPOSITORY_ROOT, "docs/plan/production-diagnostics-plan-review-record.md"),
      ),
      integration_sha: currentIntegration,
    },
    environment: reportEnvironment(browserVersion),
    exclusive_interval: {
      resource: "benchmark-host",
      lease_id_sha256: requiredEnvironment("TROUPE_STRESS_LEASE_ID"),
      holder_pid: process.pid,
      started_at_epoch_ns: requiredEnvironment("TROUPE_STRESS_EXCLUSIVE_STARTED_NS"),
      ended_at_epoch_ns: (BigInt(Date.now()) * 1_000_000n).toString(),
    },
    workload: STRESS_WORKLOAD,
    reference,
    samples: SAMPLES,
    summary,
    result: { status, violations, result_sha256: resultSha256 },
  };
}

function writeReport(report: unknown): void {
  const capturesCalibration = REPORT_KIND === "calibration"
    && process.env.TROUPE_CAPTURE_PERFORMANCE_BASELINE === "1"
    && REPORT_PATH === BASELINE_RAW_PATH;
  if (
    !capturesCalibration && REPORT_PATH.startsWith(`${REPOSITORY_ROOT}/`)
  ) {
    throw new Error("stress raw report must be an absolute path outside the repository");
  }
  if (!existsSync(dirname(REPORT_PATH)) || existsSync(REPORT_PATH)) {
    throw new Error("stress raw report parent must exist and target must be create-new");
  }
  validateSchema(report, RAW_SCHEMA);
  const temporary = `${REPORT_PATH}.tmp-${process.pid}`;
  if (existsSync(temporary)) {
    throw new Error("stress raw report staging path already exists");
  }
  writeFileSync(temporary, `${JSON.stringify(report, null, 2)}\n`, { encoding: "utf8", flag: "wx" });
  renameSync(temporary, REPORT_PATH);
}

validateBaseline();
test.describe.configure({ mode: "serial" });

let server: ViteDevServer | null = null;
let origin = "";
let browserVersion = "";

test.beforeAll(async ({ browser }: { browser: Browser }) => {
  browserVersion = browser.version();
  const cacheRoot = process.env.TROUPE_GATE_TMP ?? PROJECT_ROOT;
  server = await createServer({
    root: PROJECT_ROOT,
    cacheDir: resolve(cacheRoot, "vite-v05-chromium"),
    logLevel: "error",
    server: { host: "127.0.0.1", port: 0, strictPort: false },
    plugins: [fixturePlugin()],
  });
  await server.listen();
  const address = server.httpServer?.address();
  if (address === null || address === undefined || typeof address === "string") {
    throw new Error("V05 fixture did not bind an inet address");
  }
  origin = `http://127.0.0.1:${address.port}`;
});

test.afterAll(async () => {
  try {
    expect(SAMPLES).toHaveLength(REPEAT);
    const report = buildReport(browserVersion);
    writeReport(report);
    expect(report.result.violations, JSON.stringify(report.result.violations)).toEqual([]);
  } finally {
    await server?.close();
    server = null;
  }
});

for (let attempt = 1; attempt <= REPEAT; attempt += 1) {
  test(`bounded Chromium workload sample ${attempt}`, async ({ page }: { page: Page }) => {
    await page.goto(`${origin}/__v05`, { waitUntil: "networkidle" });
    const session = await page.context().newCDPSession(page);
    await session.send("Performance.enable");
    const before = await heapBytes(session);
    const result = await page.evaluate(() => globalThis.__v05.run()) as StressBrowserResult;
    const peak = await heapBytes(session);
    await page.evaluate(() => globalThis.__v05.release());
    const after = await heapBytes(session);
    await session.detach();
    const growth = Math.max(0, peak - before);
    const retainedBytes = Math.max(0, after - before);
    const sample: RawSample = {
      attempt,
      measured_at_epoch_ms: Date.now(),
      browser_version: browserVersion,
      durations_ms: result.durations_ms,
      heap_bytes: {
        before,
        peak,
        after,
        retained: retainedBytes,
        recovery_ratio: growth === 0 ? 0 : retainedBytes / growth,
      },
      invariants: result.invariants,
    };
    SAMPLES.push(sample);
    for (const duration of Object.values(result.durations_ms)) {
      expect(Number.isFinite(duration) && duration >= 0).toBe(true);
    }
    assertInvariants(result.invariants);
  });
}
