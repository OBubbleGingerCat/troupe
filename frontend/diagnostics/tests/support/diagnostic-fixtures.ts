import { createHash } from "node:crypto";
import { readFileSync, readdirSync } from "node:fs";
import { dirname, resolve } from "node:path";
import { fileURLToPath } from "node:url";


export interface FixtureManifestEntry {
  readonly file: string;
  readonly format: string;
  readonly sha256: string;
}

export interface FixtureManifest {
  readonly schema_version: 1;
  readonly fixtures: readonly FixtureManifestEntry[];
}

const repositoryRoot = resolve(dirname(fileURLToPath(import.meta.url)), "../../../..");
const diagnosticFixtureRoot = resolve(repositoryRoot, "tests/fixtures/diagnostics");

function readJson(path: string): unknown {
  return JSON.parse(readFileSync(path, "utf8")) as unknown;
}

function loadManifest(area: "events" | "views"): FixtureManifest {
  const root = resolve(diagnosticFixtureRoot, area);
  const manifest = readJson(resolve(root, "manifest.json")) as FixtureManifest;
  if (process.env.TROUPE_DIAGNOSTIC_AUDIT_FIXTURES === "1") {
    const listedFiles = manifest.fixtures.map((entry) => entry.file);
    const actualFiles = readdirSync(root)
      .filter((file) => file.endsWith(".json") && file !== "manifest.json")
      .sort();
    if (manifest.schema_version !== 1 || JSON.stringify(listedFiles) !== JSON.stringify(actualFiles)) {
      throw new Error(`${area} manifest does not list the exact fixture inventory`);
    }
    for (const entry of manifest.fixtures) {
      if (!/^[a-z0-9-]+\.json$/.test(entry.file) || !/^[0-9a-f]{64}$/.test(entry.sha256)) {
        throw new Error(`${area}/${entry.file} manifest metadata is not canonical`);
      }
      const digest = createHash("sha256")
        .update(readFileSync(resolve(root, entry.file)))
        .digest("hex");
      if (digest !== entry.sha256) {
        throw new Error(`${area}/${entry.file} SHA-256 mismatch`);
      }
    }
  }
  return manifest;
}

export function loadEventManifest(): FixtureManifest {
  return loadManifest("events");
}

export function loadEventFixture(file: string): unknown {
  return readJson(resolve(diagnosticFixtureRoot, "events", file));
}

export function loadHttpFixture(file: string): unknown {
  return readJson(resolve(diagnosticFixtureRoot, "http", file));
}

export function loadAllValidEventFixtures(): readonly unknown[] {
  return loadEventManifest().fixtures
    .filter((entry) => entry.format === "event_array")
    .flatMap((entry) => loadEventFixture(entry.file) as readonly unknown[]);
}

export function loadMalformedEventCases(): readonly {
  readonly name: string;
  readonly expected_error: string;
  readonly event: unknown;
}[] {
  const fixture = loadEventFixture("malformed.json") as {
    readonly cases: readonly {
      readonly name: string;
      readonly expected_error: string;
      readonly event: unknown;
    }[];
  };
  return fixture.cases;
}

export function readProtocolSource(file: string): string {
  return readFileSync(resolve(repositoryRoot, "frontend/diagnostics/src/protocol", file), "utf8");
}
