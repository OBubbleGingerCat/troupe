import { createHash } from "node:crypto";
import { readFile } from "node:fs/promises";
import { dirname, resolve } from "node:path";
import { fileURLToPath } from "node:url";

import { describe, expect, it } from "vitest";


const frontendRoot = resolve(dirname(fileURLToPath(import.meta.url)), "../..");
const manifestPath = resolve(frontendRoot, "tests/tooling/playwright-browsers.json");
const lockPath = resolve(frontendRoot, "package-lock.json");
const browsersPath = resolve(frontendRoot, "node_modules/playwright-core/browsers.json");
const hexSha256 = /^[0-9a-f]{64}$/;


async function readJson(path: string): Promise<Record<string, unknown>> {
  return JSON.parse(await readFile(path, "utf8")) as Record<string, unknown>;
}


function sha256(bytes: Buffer): string {
  return createHash("sha256").update(bytes).digest("hex");
}


describe("pinned Playwright browser manifest", () => {
  it("binds the exact package lock and playwright-core registry", async () => {
    const manifest = await readJson(manifestPath);
    const lockBytes = await readFile(lockPath);
    const lock = JSON.parse(lockBytes.toString("utf8")) as {
      packages: Record<string, { version?: string; integrity?: string }>;
    };
    const browserBytes = await readFile(browsersPath);
    const core = lock.packages["node_modules/playwright-core"];
    if (core === undefined) {
      throw new Error("playwright-core is absent from the package lock");
    }

    expect(Object.keys(manifest).sort()).toEqual([
      "lockSha256",
      "platforms",
      "playwrightCore",
      "schemaVersion",
    ]);
    expect(manifest.schemaVersion).toBe(1);
    expect(manifest.lockSha256).toBe(sha256(lockBytes));
    expect(manifest.playwrightCore).toEqual({
      version: core.version,
      integrity: core.integrity,
      browsersSha256: sha256(browserBytes),
    });
  });

  it("pins all default browser artifacts and their verified trees", async () => {
    const manifest = await readJson(manifestPath) as {
      platforms: Record<string, {
        playwrightPlatform: string;
        archives: Array<Record<string, unknown>>;
      }>;
    };
    const browsers = await readJson(browsersPath) as {
      browsers: Array<{ name: string; revision: string; browserVersion?: string }>;
    };
    const registry = new Map(browsers.browsers.map((item) => [item.name, item]));
    const platform = manifest.platforms["linux-x64"];
    if (platform === undefined) {
      throw new Error("linux-x64 is absent from the browser manifest");
    }

    expect(Object.keys(manifest.platforms)).toEqual(["linux-x64"]);
    expect(platform.playwrightPlatform).toBe("ubuntu22.04-x64");
    expect(platform.archives.map((archive) => archive.name)).toEqual([
      "chromium",
      "chromium-headless-shell",
      "firefox",
      "webkit",
      "ffmpeg",
    ]);

    for (const archive of platform.archives) {
      const name = archive.name as string;
      const expected = registry.get(name);
      expect(expected, name).toBeDefined();
      expect(Object.keys(archive).sort()).toEqual([
        "archiveSha256",
        "browserVersion",
        "cacheDirectory",
        "executable",
        "executableSha256",
        "materializedLinks",
        "memberCount",
        "name",
        "revision",
        "treeSha256",
        "url",
      ]);
      expect(archive.revision).toBe(expected?.revision);
      expect(archive.browserVersion ?? null).toBe(expected?.browserVersion ?? null);
      expect(archive.url).toMatch(/^https:\/\/(cdn\.playwright\.dev|playwright\.download\.prss\.microsoft\.com)\//);
      expect(archive.archiveSha256).toMatch(hexSha256);
      expect(archive.treeSha256).toMatch(hexSha256);
      expect(archive.executableSha256).toMatch(hexSha256);
      expect(archive.cacheDirectory).toBe(`${name.replace(/-/g, "_")}-${archive.revision as string}`);
      expect(archive.executable).toMatch(/^[^/]+(?:\/[^/]+)*$/);
      expect(archive.memberCount).toBeGreaterThan(0);
      expect(Array.isArray(archive.materializedLinks)).toBe(true);
    }
    expect(platform.archives.slice(0, 3).every((archive) => (
      (archive.materializedLinks as unknown[]).length === 0
    ))).toBe(true);
    expect((platform.archives[3]?.materializedLinks as unknown[]).length).toBeGreaterThan(0);
    expect((platform.archives[4]?.materializedLinks as unknown[]).length).toBe(0);
  });
});
