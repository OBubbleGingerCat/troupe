import { readFileSync } from "node:fs";
import { resolve } from "node:path";

import {
  cleanup,
  render,
  screen,
} from "@testing-library/preact";
import "@testing-library/jest-dom/vitest";
import { createElement } from "preact";
import {
  afterEach,
  describe,
  expect,
  it,
} from "vitest";

import { App } from "../../src/app.tsx";
import type { DiagnosticFetch } from "../../src/live/bootstrap.ts";
import { createLiveDiagnosticsController } from "../../src/live/reconnect.ts";


const frontendRoot = resolve(process.cwd());
const runId = "12345678-1234-4234-9234-123456789abc";


function identity() {
  return {
    identity_schema_version: 1,
    server_protocol_version: 1,
    event_schema_version: 1,
    api_schema_version: 1,
    run_id: runId,
    owner_pid: 1234,
    process_identity: "process-identity",
    bind_host: "0.0.0.0",
    port: 43123,
    local_endpoint: "http://127.0.0.1:43123/troupe/",
    advertise_url: null,
    base_path: "/troupe",
    api_base_path: "/troupe/api/v1",
    identity_path: "/troupe/api/v1/identity",
    security_scope: "trusted_network",
    operational_limits: {},
  };
}


function status() {
  return {
    api_schema_version: 1,
    run_id: runId,
    source: "active",
    store_schema_version: "1",
    store_schema_identity: "store-v1",
    event_schema_version: "1",
    configuration_identity: "configuration-v1",
    event_watermark: "0",
    read_model_watermark: "0",
    lifecycle: {
      state: "active",
      started_at: "2026-08-16T00:00:00Z",
      ended_at: null,
      outcome: null,
      clean_shutdown: false,
    },
    writer: { status: "unavailable", reason: "state_unavailable" },
    quota: { status: "unavailable", reason: "state_unavailable" },
  };
}


afterEach(() => {
  cleanup();
  vi.unstubAllGlobals();
});


describe("deterministic raw bundle contract", () => {
  it("keeps the release configuration single-entry, relative, and ES2020", () => {
    const config = readFileSync(resolve(frontendRoot, "vite.config.ts"), "utf8");
    const entry = readFileSync(resolve(frontendRoot, "index.html"), "utf8");
    const document = new DOMParser().parseFromString(entry, "text/html");

    expect(config).toContain('base: "./"');
    expect(config).toContain('target: "es2020"');
    expect(config).toContain("cssCodeSplit: false");
    expect(config).toContain("sourcemap: false");
    expect(config).toContain("modulePreload: false");
    expect(config).toContain("inlineDynamicImports: true");
    expect(document.querySelectorAll("script")).toHaveLength(1);
    expect(document.querySelector("script")?.getAttribute("src")).toBe("./src/main.tsx");
    expect(document.querySelector("script")?.textContent?.trim()).toBe("");
    expect(document.querySelectorAll('link[rel="icon"]')).toHaveLength(1);
    expect(document.querySelector('link[rel="icon"]')?.getAttribute("href")).toBe("data:,");
    expect(document.querySelector("style, base, link[href^='http']")).toBeNull();
  });

  it("renders only static compatibility without snapshot, query, or live transport", async () => {
    vi.stubGlobal("EventSource", undefined);
    const urls: string[] = [];
    const fetch: DiagnosticFetch = async (input) => {
      const url = new URL(input.toString());
      urls.push(url.href);
      const body = url.pathname.endsWith("/identity") ? identity() : status();
      return new Response(JSON.stringify(body), {
        status: 200,
        headers: { "content-type": "application/json; charset=utf-8" },
      });
    };
    const controller = createLiveDiagnosticsController({
      baseUrl: "http://diagnostics.test/troupe/",
      fetch,
    });
    await controller.start();
    render(createElement(App, { liveController: controller }));

    expect(controller.state.phase).toBe("compatibility");
    expect(controller.state.bootstrap?.compatibility).toMatchObject({
      mode: "static",
      missingBrowserCapabilities: ["EventSource"],
    });
    expect(screen.getByLabelText("Compatibility status")).toHaveTextContent(
      "Required browser capabilities are unavailable",
    );
    expect(urls).toEqual([
      "http://diagnostics.test/troupe/api/v1/identity",
      "http://diagnostics.test/troupe/api/v1/status",
    ]);
    expect(urls.every((url) => !/\/(snapshot|events)(?:[/?]|$)/.test(url))).toBe(true);
    controller.stop();
  });
});
