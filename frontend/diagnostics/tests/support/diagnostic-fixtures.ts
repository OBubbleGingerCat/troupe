import { readFileSync, readdirSync } from "node:fs";
import { dirname, resolve } from "node:path";
import { fileURLToPath } from "node:url";


const repositoryRoot = resolve(dirname(fileURLToPath(import.meta.url)), "../../../..");
const diagnosticFixtureRoot = resolve(repositoryRoot, "tests/fixtures/diagnostics");
const eventFixtureRoot = resolve(diagnosticFixtureRoot, "events");

function readJson(path: string): unknown {
  return JSON.parse(readFileSync(path, "utf8")) as unknown;
}

export function loadEventFixture(file: string): unknown {
  return readJson(resolve(eventFixtureRoot, file));
}

export function loadHttpFixture(file: string): unknown {
  return readJson(resolve(diagnosticFixtureRoot, "http", file));
}

export function loadAllValidEventFixtures(): readonly unknown[] {
  return readdirSync(eventFixtureRoot)
    .filter((file) => file.endsWith(".json") && file !== "malformed.json")
    .sort()
    .flatMap((file) => loadEventFixture(file) as readonly unknown[]);
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
