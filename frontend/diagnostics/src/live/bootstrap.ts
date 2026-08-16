import {
  SUPPORTED_SCHEMA_VERSIONS,
  evaluateProtocolCompatibility,
  type ProtocolCompatibilityResult,
} from "../protocol/compatibility.ts";
import {
  type CanonicalUuid,
  type JsonObject,
  type U64String,
  decodeCanonicalUuid,
  decodeJsonValue,
  decodeU64,
  expectBoolean,
  expectEnum,
  expectExactFields,
  expectInteger,
  expectObject,
  expectString,
  failProtocol,
  parseProtocolJson,
} from "../protocol/decimal.ts";
import { decodeHttpErrorResponse } from "../protocol/http.ts";
import type { EventSourceConstructor } from "./sse.ts";


export const IDENTITY_RELATIVE_PATH = "api/v1/identity";

export type DiagnosticFetch = typeof globalThis.fetch;
export type SecurityScope = "trusted_network";
export type ProductionState = "active" | "completed" | "failed" | "incomplete";
export type ProductionOutcome = "completed" | "failed" | "cancelled";

export interface ServerIdentity {
  readonly identity_schema_version: number;
  readonly server_protocol_version: number;
  readonly event_schema_version: number;
  readonly view_schema_version: number;
  readonly api_schema_version: number;
  readonly run_id: CanonicalUuid;
  readonly owner_pid: number;
  readonly process_identity: string;
  readonly bind_host: string;
  readonly port: number;
  readonly local_endpoint: string;
  readonly advertise_url: string | null;
  readonly base_path: string;
  readonly api_base_path: string;
  readonly identity_path: string;
  readonly security_scope: SecurityScope;
  readonly operational_limits: Readonly<Record<string, U64String>>;
}

export interface ProductionLifecycle {
  readonly state: ProductionState;
  readonly started_at: string;
  readonly ended_at: string | null;
  readonly outcome: ProductionOutcome | null;
  readonly clean_shutdown: boolean;
}

export interface DiagnosticStatus {
  readonly api_schema_version: 1;
  readonly run_id: CanonicalUuid;
  readonly source: "active" | "archive";
  readonly store_schema_version: U64String;
  readonly store_schema_identity: string;
  readonly event_schema_version: U64String;
  readonly configuration_identity: string;
  readonly event_watermark: U64String;
  readonly read_model_watermark: U64String;
  readonly lifecycle: ProductionLifecycle;
  readonly writer: JsonObject;
  readonly quota: JsonObject;
}

export interface DiagnosticBootstrap {
  readonly document_url: string;
  readonly origin: string;
  readonly api_base_url: string;
  readonly identity: ServerIdentity;
  readonly status: DiagnosticStatus;
  readonly compatibility: ProtocolCompatibilityResult;
}

export interface BootstrapOptions {
  readonly baseUrl?: string | URL;
  readonly fetch?: DiagnosticFetch;
  readonly EventSource?: EventSourceConstructor;
}

export type DiagnosticTransportErrorCode =
  | "browser_capability"
  | "cross_origin"
  | "http"
  | "identity"
  | "protocol"
  | "security";

export class DiagnosticTransportError extends Error {
  readonly code: DiagnosticTransportErrorCode;
  readonly status: number | null;

  constructor(code: DiagnosticTransportErrorCode, message: string, status: number | null = null) {
    super(message);
    this.name = "DiagnosticTransportError";
    this.code = code;
    this.status = status;
  }
}

const IDENTITY_FIELDS = [
  "identity_schema_version",
  "server_protocol_version",
  "event_schema_version",
  "view_schema_version",
  "api_schema_version",
  "run_id",
  "owner_pid",
  "process_identity",
  "bind_host",
  "port",
  "local_endpoint",
  "advertise_url",
  "base_path",
  "api_base_path",
  "identity_path",
  "security_scope",
  "operational_limits",
] as const;

const STATUS_FIELDS = [
  "api_schema_version",
  "run_id",
  "source",
  "store_schema_version",
  "store_schema_identity",
  "event_schema_version",
  "configuration_identity",
  "event_watermark",
  "read_model_watermark",
  "lifecycle",
  "writer",
  "quota",
] as const;

function positiveVersion(value: unknown, path: string): number {
  const version = expectInteger(value, path);
  if (version < 1) {
    failProtocol("schema_version", path, "expected a positive integer");
  }
  return version;
}

function nonemptyString(value: unknown, path: string): string {
  const text = expectString(value, path);
  if (text.length === 0) {
    failProtocol("string", path, "expected a nonempty string");
  }
  return text;
}

function nullableString(value: unknown, path: string): string | null {
  return value === null ? null : nonemptyString(value, path);
}

function absoluteHttpUrl(value: unknown, path: string): string {
  const text = nonemptyString(value, path);
  let parsed: URL;
  try {
    parsed = new URL(text);
  } catch {
    failProtocol("url", path, "expected an absolute HTTP(S) URL");
  }
  if (!matchesHttpProtocol(parsed.protocol) || parsed.username !== "" || parsed.password !== "") {
    failProtocol("url", path, "expected an absolute HTTP(S) URL without user information");
  }
  return text;
}

function matchesHttpProtocol(protocol: string): boolean {
  return protocol === "http:" || protocol === "https:";
}

function absolutePath(value: unknown, path: string): string {
  const text = nonemptyString(value, path);
  if (!text.startsWith("/") || text.includes("?") || text.includes("#") || text.includes("//")) {
    failProtocol("path", path, "expected a normalized absolute path");
  }
  if (text !== "/" && text.endsWith("/")) {
    failProtocol("path", path, "non-root paths must not end with a slash");
  }
  if (text.split("/").some((part) => part === "." || part === "..")) {
    failProtocol("path", path, "path traversal segments are not allowed");
  }
  return text;
}

function decodeOperationalLimits(value: unknown, path: string): Readonly<Record<string, U64String>> {
  const object = expectObject(value, path);
  const result: Record<string, U64String> = {};
  for (const [name, raw] of Object.entries(object)) {
    if (!/^[a-z][a-z0-9_]{0,127}$/.test(name)) {
      failProtocol("operational_limit", `${path}.${name}`, "invalid operational limit name");
    }
    result[name] = decodeU64(raw, `${path}.${name}`);
  }
  return result;
}

export function decodeServerIdentity(value: unknown, path = "identity"): ServerIdentity {
  const identity = expectObject(value, path);
  expectExactFields(identity, IDENTITY_FIELDS, path);
  const identityVersion = positiveVersion(
    identity.identity_schema_version,
    `${path}.identity_schema_version`,
  );
  if (identityVersion !== 1) {
    failProtocol("identity_schema_version", `${path}.identity_schema_version`, "expected integer 1");
  }
  const ownerPid = expectInteger(identity.owner_pid, `${path}.owner_pid`);
  if (ownerPid < 1) {
    failProtocol("pid", `${path}.owner_pid`, "expected a positive process ID");
  }
  const port = expectInteger(identity.port, `${path}.port`);
  if (port < 1 || port > 65_535) {
    failProtocol("port", `${path}.port`, "expected a nonzero TCP port");
  }
  const basePath = absolutePath(identity.base_path, `${path}.base_path`);
  const apiBasePath = absolutePath(identity.api_base_path, `${path}.api_base_path`);
  const identityPath = absolutePath(identity.identity_path, `${path}.identity_path`);
  const expectedApiBasePath = basePath === "/" ? "/api/v1" : `${basePath}/api/v1`;
  if (apiBasePath !== expectedApiBasePath || identityPath !== `${apiBasePath}/identity`) {
    failProtocol("identity", path, "advertised API paths are inconsistent");
  }
  const advertiseUrl = identity.advertise_url === null
    ? null
    : absoluteHttpUrl(identity.advertise_url, `${path}.advertise_url`);
  return {
    identity_schema_version: 1,
    server_protocol_version: positiveVersion(
      identity.server_protocol_version,
      `${path}.server_protocol_version`,
    ),
    event_schema_version: positiveVersion(
      identity.event_schema_version,
      `${path}.event_schema_version`,
    ),
    view_schema_version: positiveVersion(
      identity.view_schema_version,
      `${path}.view_schema_version`,
    ),
    api_schema_version: positiveVersion(
      identity.api_schema_version,
      `${path}.api_schema_version`,
    ),
    run_id: decodeCanonicalUuid(identity.run_id, `${path}.run_id`),
    owner_pid: ownerPid,
    process_identity: nonemptyString(identity.process_identity, `${path}.process_identity`),
    bind_host: nonemptyString(identity.bind_host, `${path}.bind_host`),
    port,
    local_endpoint: absoluteHttpUrl(identity.local_endpoint, `${path}.local_endpoint`),
    advertise_url: advertiseUrl,
    base_path: basePath,
    api_base_path: apiBasePath,
    identity_path: identityPath,
    security_scope: expectEnum(
      identity.security_scope,
      ["trusted_network"],
      `${path}.security_scope`,
      "security_scope",
    ),
    operational_limits: decodeOperationalLimits(
      identity.operational_limits,
      `${path}.operational_limits`,
    ),
  };
}

function decodeLifecycle(value: unknown, path: string): ProductionLifecycle {
  const lifecycle = expectObject(value, path);
  expectExactFields(
    lifecycle,
    ["state", "started_at", "ended_at", "outcome", "clean_shutdown"],
    path,
  );
  const state = expectEnum(
    lifecycle.state,
    ["active", "completed", "failed", "incomplete"],
    `${path}.state`,
  );
  const outcome = lifecycle.outcome === null
    ? null
    : expectEnum(
      lifecycle.outcome,
      ["completed", "failed", "cancelled"],
      `${path}.outcome`,
    );
  const result = {
    state,
    started_at: nonemptyString(lifecycle.started_at, `${path}.started_at`),
    ended_at: nullableString(lifecycle.ended_at, `${path}.ended_at`),
    outcome,
    clean_shutdown: expectBoolean(lifecycle.clean_shutdown, `${path}.clean_shutdown`),
  } satisfies ProductionLifecycle;
  if (state === "active" && (result.ended_at !== null || outcome !== null)) {
    failProtocol("lifecycle", path, "active lifecycle must not have terminal metadata");
  }
  if (state === "completed" && outcome !== "completed") {
    failProtocol("lifecycle", path, "completed lifecycle requires completed outcome");
  }
  if (state === "failed" && outcome !== "failed" && outcome !== "cancelled") {
    failProtocol("lifecycle", path, "failed lifecycle requires failed or cancelled outcome");
  }
  if (state === "incomplete" && (outcome !== null || result.clean_shutdown)) {
    failProtocol("lifecycle", path, "incomplete lifecycle cannot claim a clean terminal outcome");
  }
  return result;
}

function decodeObservation(value: unknown, path: string): JsonObject {
  const observation = expectObject(value, path);
  const status = expectEnum(observation.status, ["available", "unavailable"], `${path}.status`);
  expectExactFields(
    observation,
    status === "available" ? ["status", "value"] : ["status", "reason"],
    path,
  );
  if (status === "available") {
    decodeJsonValue(expectObject(observation.value, `${path}.value`), `${path}.value`);
  } else {
    nonemptyString(observation.reason, `${path}.reason`);
  }
  return decodeJsonValue(observation, path) as JsonObject;
}

export function decodeDiagnosticStatus(value: unknown, path = "status"): DiagnosticStatus {
  const status = expectObject(value, path);
  expectExactFields(status, STATUS_FIELDS, path);
  if (status.api_schema_version !== 1) {
    failProtocol("api_schema_version", `${path}.api_schema_version`, "expected integer 1");
  }
  const eventWatermark = decodeU64(status.event_watermark, `${path}.event_watermark`);
  const readModelWatermark = decodeU64(
    status.read_model_watermark,
    `${path}.read_model_watermark`,
  );
  if (BigInt(readModelWatermark) > BigInt(eventWatermark)) {
    failProtocol("watermark", path, "read-model watermark exceeds the event watermark");
  }
  return {
    api_schema_version: 1,
    run_id: decodeCanonicalUuid(status.run_id, `${path}.run_id`),
    source: expectEnum(status.source, ["active", "archive"], `${path}.source`),
    store_schema_version: decodeU64(
      status.store_schema_version,
      `${path}.store_schema_version`,
    ),
    store_schema_identity: nonemptyString(
      status.store_schema_identity,
      `${path}.store_schema_identity`,
    ),
    event_schema_version: decodeU64(
      status.event_schema_version,
      `${path}.event_schema_version`,
    ),
    configuration_identity: nonemptyString(
      status.configuration_identity,
      `${path}.configuration_identity`,
    ),
    event_watermark: eventWatermark,
    read_model_watermark: readModelWatermark,
    lifecycle: decodeLifecycle(status.lifecycle, `${path}.lifecycle`),
    writer: decodeObservation(status.writer, `${path}.writer`),
    quota: decodeObservation(status.quota, `${path}.quota`),
  };
}

export function resolveDocumentUrl(value?: string | URL): URL {
  const candidate = value ?? (
    typeof document === "undefined" ? undefined : document.baseURI
  );
  if (candidate === undefined) {
    throw new DiagnosticTransportError(
      "browser_capability",
      "a browser document URL is required for same-origin diagnostics",
    );
  }
  let url: URL;
  try {
    url = new URL(candidate.toString());
  } catch {
    throw new DiagnosticTransportError("cross_origin", "diagnostic document URL is invalid");
  }
  if (!matchesHttpProtocol(url.protocol)) {
    throw new DiagnosticTransportError(
      "cross_origin",
      "diagnostics require an HTTP(S) same-origin document",
    );
  }
  return url;
}

export function assertSameOriginUrl(url: URL, expectedOrigin: string): void {
  if (!matchesHttpProtocol(url.protocol) || url.origin !== expectedOrigin) {
    throw new DiagnosticTransportError(
      "cross_origin",
      `refusing external diagnostic request to ${url.origin}`,
    );
  }
}

export function diagnosticApiUrl(
  bootstrap: Pick<DiagnosticBootstrap, "origin" | "api_base_url">,
  resource: string,
): URL {
  if (!/^[a-z][a-z0-9_-]*$/.test(resource)) {
    throw new DiagnosticTransportError("protocol", "diagnostic API resource name is invalid");
  }
  const url = new URL(resource, bootstrap.api_base_url);
  assertSameOriginUrl(url, bootstrap.origin);
  return url;
}

function requireFetch(candidate: DiagnosticFetch | undefined): DiagnosticFetch {
  if (typeof candidate !== "function") {
    throw new DiagnosticTransportError("browser_capability", "native fetch is unavailable");
  }
  return candidate;
}

export async function fetchSameOriginJson(
  url: URL,
  expectedOrigin: string,
  fetchImpl: DiagnosticFetch,
  label: string,
): Promise<unknown> {
  assertSameOriginUrl(url, expectedOrigin);
  let response: Response;
  try {
    response = await fetchImpl(url, {
      method: "GET",
      headers: { Accept: "application/json" },
      cache: "no-store",
      credentials: "same-origin",
      redirect: "error",
    });
  } catch (error) {
    throw new DiagnosticTransportError(
      "http",
      `${label} request failed: ${error instanceof Error ? error.message : String(error)}`,
    );
  }
  const text = await response.text();
  const value = parseProtocolJson(text, label);
  if (!response.ok) {
    let detail = `${label} returned HTTP ${response.status}`;
    try {
      const decoded = decodeHttpErrorResponse(value, `${label}.error`);
      detail = `${detail}: ${decoded.error.code}: ${decoded.error.message}`;
    } catch {
      // Preserve the transport status when an error body is itself malformed.
    }
    throw new DiagnosticTransportError("http", detail, response.status);
  }
  const contentType = response.headers.get("content-type");
  if (contentType !== null && !contentType.toLowerCase().startsWith("application/json")) {
    throw new DiagnosticTransportError("protocol", `${label} did not return JSON`);
  }
  return value;
}

function missingCapabilities(
  fetchImpl: DiagnosticFetch | undefined,
  EventSourceImpl: EventSourceConstructor | undefined,
): readonly string[] {
  const missing: string[] = [];
  if (typeof fetchImpl !== "function") {
    missing.push("fetch");
  }
  if (typeof EventSourceImpl !== "function") {
    missing.push("EventSource");
  }
  if (typeof URL !== "function") {
    missing.push("URL");
  }
  if (typeof BigInt !== "function") {
    missing.push("BigInt");
  }
  return missing;
}

export async function fetchDiagnosticStatus(
  bootstrap: Pick<DiagnosticBootstrap, "origin" | "api_base_url" | "identity">,
  fetchImpl: DiagnosticFetch = globalThis.fetch,
): Promise<DiagnosticStatus> {
  const value = await fetchSameOriginJson(
    diagnosticApiUrl(bootstrap, "status"),
    bootstrap.origin,
    requireFetch(fetchImpl),
    "diagnostic status",
  );
  const status = decodeDiagnosticStatus(value);
  if (status.run_id !== bootstrap.identity.run_id) {
    throw new DiagnosticTransportError("identity", "status belongs to another Run");
  }
  if (BigInt(status.event_schema_version) !== BigInt(bootstrap.identity.event_schema_version)) {
    throw new DiagnosticTransportError("identity", "status event schema differs from server identity");
  }
  return status;
}

export async function bootstrapDiagnostics(
  options: BootstrapOptions = {},
): Promise<DiagnosticBootstrap> {
  const documentUrl = resolveDocumentUrl(options.baseUrl);
  const origin = documentUrl.origin;
  const fetchImpl = options.fetch ?? globalThis.fetch;
  const EventSourceImpl = options.EventSource ?? globalThis.EventSource;
  const usableFetch = requireFetch(fetchImpl);
  const identityUrl = new URL(IDENTITY_RELATIVE_PATH, documentUrl);
  assertSameOriginUrl(identityUrl, origin);
  const identity = decodeServerIdentity(await fetchSameOriginJson(
    identityUrl,
    origin,
    usableFetch,
    "diagnostic identity",
  ));
  if (identity.identity_path !== identityUrl.pathname) {
    throw new DiagnosticTransportError(
      "identity",
      "server identity path differs from the same-origin bootstrap route",
    );
  }
  const apiBaseUrl = new URL(`${identity.api_base_path}/`, documentUrl);
  assertSameOriginUrl(apiBaseUrl, origin);
  const compatibility = evaluateProtocolCompatibility(
    {
      event: identity.event_schema_version,
      api: identity.api_schema_version,
      control: identity.server_protocol_version,
      view: identity.view_schema_version,
      ui: SUPPORTED_SCHEMA_VERSIONS.ui,
    },
    { missingBrowserCapabilities: missingCapabilities(fetchImpl, EventSourceImpl) },
  );
  const partial = {
    document_url: documentUrl.href,
    origin,
    api_base_url: apiBaseUrl.href,
    identity,
  };
  const status = await fetchDiagnosticStatus(
    partial,
    usableFetch,
  );
  return { ...partial, status, compatibility };
}
