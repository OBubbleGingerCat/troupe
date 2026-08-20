import { expectInteger, failProtocol } from "./decimal.ts";


export const SUPPORTED_SCHEMA_VERSIONS = Object.freeze({
  event: 1,
  api: 1,
  control: 1,
  ui: 1,
} as const);

export type SchemaDomain = keyof typeof SUPPORTED_SCHEMA_VERSIONS;
export type SchemaVersions = Readonly<Record<SchemaDomain, number>>;

export type VersionCompatibilityDecision =
  | {
    readonly domain: SchemaDomain;
    readonly status: "compatible";
    readonly supported: number;
    readonly received: number;
  }
  | {
    readonly domain: SchemaDomain;
    readonly status: "incompatible";
    readonly supported: number;
    readonly received: number;
    readonly reason: "major_version_mismatch";
  };

export interface ProtocolCompatibilityResult {
  readonly mode: "interactive" | "static";
  readonly decisions: Readonly<Record<SchemaDomain, VersionCompatibilityDecision>>;
  readonly missingBrowserCapabilities: readonly string[];
}

export function checkSchemaCompatibility(
  domain: SchemaDomain,
  received: unknown,
  supported: number = SUPPORTED_SCHEMA_VERSIONS[domain],
): VersionCompatibilityDecision {
  const version = expectInteger(received, `${domain}_schema_version`);
  if (version < 1 || supported < 1 || !Number.isSafeInteger(supported)) {
    failProtocol("schema_version", `${domain}_schema_version`, "version must be a positive integer");
  }
  if (version === supported) {
    return { domain, status: "compatible", supported, received: version };
  }
  return {
    domain,
    status: "incompatible",
    supported,
    received: version,
    reason: "major_version_mismatch",
  };
}

export function evaluateProtocolCompatibility(
  received: SchemaVersions,
  options: {
    readonly supported?: SchemaVersions;
    readonly missingBrowserCapabilities?: readonly string[];
  } = {},
): ProtocolCompatibilityResult {
  const supported = options.supported ?? SUPPORTED_SCHEMA_VERSIONS;
  const missingBrowserCapabilities = [...(options.missingBrowserCapabilities ?? [])];
  const decisions = {
    event: checkSchemaCompatibility("event", received.event, supported.event),
    api: checkSchemaCompatibility("api", received.api, supported.api),
    control: checkSchemaCompatibility("control", received.control, supported.control),
    ui: checkSchemaCompatibility("ui", received.ui, supported.ui),
  } satisfies Record<SchemaDomain, VersionCompatibilityDecision>;
  const incompatible = Object.values(decisions).some(
    (decision) => decision.status === "incompatible",
  );
  return {
    mode: incompatible || missingBrowserCapabilities.length > 0 ? "static" : "interactive",
    decisions,
    missingBrowserCapabilities,
  };
}
