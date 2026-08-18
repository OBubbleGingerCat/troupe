import {
  type CanonicalUuid,
  type JsonObject,
  type U64String,
  ProtocolDecodeError,
  decodeCanonicalUuid,
  expectArray,
  expectEnum,
  expectExactFields,
  expectInteger,
  expectObject,
  expectString,
  failProtocol,
  parseProtocolJson,
} from "../protocol/decimal.ts";
import { decodeHttpErrorResponse } from "../protocol/http.ts";
import {
  VIEW_RENDERERS,
  type TimeSeriesViewResponse,
  type ViewCapabilities,
  type ViewCoverage,
  type ViewRecord,
  type ViewRenderer,
  type ViewResponse,
  decodeViewCapabilities,
  decodeViewRecord,
  decodeViewResponse,
} from "../protocol/view.ts";
import {
  type DiagnosticBootstrap,
  type DiagnosticFetch,
  type ServerIdentity,
  assertSameOriginUrl,
  diagnosticApiUrl,
} from "../live/bootstrap.ts";
import {
  BoundedViewQueryCache,
  StaleViewQueryError,
} from "./cache.ts";
import {
  type ViewQueryContext,
  type ViewQueryGeneration,
  appendViewBindingParameters,
  assertViewResponseGeneration,
  freezeViewQueryGeneration,
  isTimeSeriesResponse,
} from "./binding.ts";
import {
  type FrozenViewPagination,
  type ViewPaginationInput,
  appendViewPaginationParameters,
  assertViewResponsePagination,
  freezeViewPagination,
  nextViewPagination,
  viewPaginationKey,
} from "./pagination.ts";


const VIEW_CATALOG_LIMIT = 64;
const DEFAULT_REQUEST_TIMEOUT_MS = 30_000;

export interface IncompatibleViewCatalogEntry {
  readonly status: "incompatible";
  readonly view_id: string;
  readonly renderer: ViewRenderer;
  readonly incompatible: {
    readonly reason: "newer_view_schema" | "corrupt_record";
    readonly supported_view_schema_version: 1;
    readonly record_view_schema_version: number | null;
  };
}

export type ViewCatalogEntry = ViewRecord | IncompatibleViewCatalogEntry;

export interface ViewCatalog {
  readonly api_schema_version: 1;
  readonly run_id: CanonicalUuid;
  readonly capabilities: ViewCapabilities;
  readonly views: readonly ViewCatalogEntry[];
}

export type ViewCatalogState =
  | { readonly status: "idle"; readonly catalog: null; readonly error: null }
  | { readonly status: "loading"; readonly catalog: null; readonly error: null }
  | { readonly status: "ready"; readonly catalog: ViewCatalog; readonly error: null }
  | { readonly status: "local_error"; readonly catalog: null; readonly error: ViewQueryLocalError };

export type ViewQueryLocalErrorCode =
  | "binding"
  | "catalog"
  | "http"
  | "identity"
  | "incompatible"
  | "pagination"
  | "protocol"
  | "query"
  | "renderer"
  | "stale"
  | "timeout"
  | "transport"
  | "view_not_found";

export class ViewQueryLocalError extends Error {
  readonly code: ViewQueryLocalErrorCode;
  readonly status: number | null;

  constructor(code: ViewQueryLocalErrorCode, message: string, status: number | null = null) {
    super(message);
    this.name = "ViewQueryLocalError";
    this.code = code;
    this.status = status;
  }
}

export interface TimeSeriesColumnarSeries {
  readonly group: JsonObject | null;
  readonly values: readonly (JsonObject | null)[];
  readonly coverage: readonly ViewCoverage[];
}

export interface TimeSeriesColumnarModel {
  readonly range_start_ns: U64String;
  readonly range_end_ns: U64String;
  readonly captured_watermark: U64String;
  readonly captured_elapsed_end_ns: U64String;
  readonly bucket_width_ns: U64String;
  readonly bucket_start_ns: readonly U64String[];
  readonly bucket_end_ns: readonly U64String[];
  readonly partial: readonly boolean[];
  readonly series: readonly TimeSeriesColumnarSeries[];
  readonly coverage: ViewCoverage;
  readonly truncated: boolean;
}

export interface ViewQueryResult {
  readonly generation: ViewQueryGeneration;
  readonly pagination: FrozenViewPagination;
  readonly response: ViewResponse;
  readonly time_series: TimeSeriesColumnarModel | null;
}

export type ViewQueryState =
  | { readonly status: "idle"; readonly generation_key: null; readonly result: null; readonly error: null }
  | {
    readonly status: "loading";
    readonly generation_key: string;
    readonly result: null;
    readonly error: null;
  }
  | {
    readonly status: "ready";
    readonly generation_key: string;
    readonly result: ViewQueryResult;
    readonly error: null;
  }
  | {
    readonly status: "local_error";
    readonly generation_key: string | null;
    readonly result: ViewQueryResult | null;
    readonly error: ViewQueryLocalError;
  };

export interface ViewQueryClientOptions {
  readonly bootstrap: Pick<DiagnosticBootstrap, "origin" | "api_base_url"> & {
    readonly identity: Pick<ServerIdentity, "run_id">;
  };
  readonly fetch?: DiagnosticFetch;
  readonly request_timeout_ms?: number;
}

function decodeViewId(value: unknown, path: string): string {
  const id = expectString(value, path);
  if (!/^[a-z][a-z0-9_]*$/.test(id) || id.length > 64) {
    failProtocol("view_id", path, "expected canonical view identifier with at most 64 bytes");
  }
  return id;
}

function decodeIncompatibleCatalogEntry(
  value: unknown,
  path: string,
): IncompatibleViewCatalogEntry {
  const entry = expectObject(value, path);
  expectExactFields(entry, ["status", "view_id", "renderer", "incompatible"], path);
  const status = expectEnum(entry.status, ["incompatible"], `${path}.status`);
  const viewId = decodeViewId(entry.view_id, `${path}.view_id`);
  const renderer = expectEnum(entry.renderer, VIEW_RENDERERS, `${path}.renderer`);
  const incompatible = expectObject(entry.incompatible, `${path}.incompatible`);
  expectExactFields(
    incompatible,
    ["reason", "supported_view_schema_version", "record_view_schema_version"],
    `${path}.incompatible`,
  );
  const reason = expectEnum(
    incompatible.reason,
    ["newer_view_schema", "corrupt_record"],
    `${path}.incompatible.reason`,
  );
  if (incompatible.supported_view_schema_version !== 1) {
    failProtocol(
      "view_schema_version",
      `${path}.incompatible.supported_view_schema_version`,
      "expected integer 1",
    );
  }
  const rawVersion = incompatible.record_view_schema_version;
  const version = rawVersion === null
    ? null
    : expectInteger(rawVersion, `${path}.incompatible.record_view_schema_version`);
  if (
    (reason === "newer_view_schema" && (version === null || version <= 1))
    || (reason === "corrupt_record" && version !== null && version > 1)
  ) {
    failProtocol(
      "view_schema_version",
      `${path}.incompatible`,
      "incompatible reason and record version disagree",
    );
  }
  return {
    status,
    view_id: viewId,
    renderer,
    incompatible: {
      reason,
      supported_view_schema_version: 1,
      record_view_schema_version: version,
    },
  };
}

export function isCompatibleViewCatalogEntry(entry: ViewCatalogEntry): entry is ViewRecord {
  return !("status" in entry);
}

export function viewCatalogEntryId(entry: ViewCatalogEntry): string {
  return isCompatibleViewCatalogEntry(entry) ? entry.id : entry.view_id;
}

export function decodeViewCatalog(value: unknown, path = "catalog"): ViewCatalog {
  const catalog = expectObject(value, path);
  expectExactFields(catalog, ["api_schema_version", "run_id", "capabilities", "views"], path);
  if (catalog.api_schema_version !== 1) {
    failProtocol("api_schema_version", `${path}.api_schema_version`, "expected integer 1");
  }
  const runId = decodeCanonicalUuid(catalog.run_id, `${path}.run_id`);
  const capabilities = decodeViewCapabilities(catalog.capabilities, `${path}.capabilities`);
  const rawViews = expectArray(catalog.views, `${path}.views`);
  if (rawViews.length > VIEW_CATALOG_LIMIT) {
    failProtocol("view_catalog", `${path}.views`, "catalog contains more than 64 views");
  }
  const views: ViewCatalogEntry[] = [];
  const ids = new Set<string>();
  rawViews.forEach((raw, index) => {
    const itemPath = `${path}.views[${index}]`;
    const object = expectObject(raw, itemPath);
    const entry = Object.prototype.hasOwnProperty.call(object, "status")
      ? decodeIncompatibleCatalogEntry(raw, itemPath)
      : decodeViewRecord(raw, itemPath);
    const id = viewCatalogEntryId(entry);
    if (ids.has(id)) {
      failProtocol("view_id", itemPath, `duplicate catalog view ${JSON.stringify(id)}`);
    }
    ids.add(id);
    views.push(entry);
  });
  return {
    api_schema_version: 1,
    run_id: runId,
    capabilities,
    views,
  };
}

export function toTimeSeriesColumnarModel(
  response: TimeSeriesViewResponse,
): TimeSeriesColumnarModel {
  const points = response.series[0]?.points ?? [];
  return {
    range_start_ns: response.binding.range_start_ns,
    range_end_ns: response.binding.range_end_ns,
    captured_watermark: response.binding.captured_watermark,
    captured_elapsed_end_ns: response.binding.captured_elapsed_end_ns,
    bucket_width_ns: response.bucket_width_ns,
    bucket_start_ns: points.map((point) => point.bucket_start_ns),
    bucket_end_ns: points.map((point) => point.bucket_end_ns),
    partial: points.map((point) => point.partial),
    series: response.series.map((series) => ({
      group: series.group,
      values: series.points.map((point) => point.value),
      coverage: series.points.map((point) => point.coverage),
    })),
    coverage: response.coverage,
    truncated: response.truncated,
  };
}

function localizeError(
  error: unknown,
  fallback: ViewQueryLocalErrorCode,
): ViewQueryLocalError {
  if (error instanceof ViewQueryLocalError) {
    return error;
  }
  if (error instanceof StaleViewQueryError) {
    return new ViewQueryLocalError("stale", error.message);
  }
  if (error instanceof ProtocolDecodeError) {
    return new ViewQueryLocalError(fallback, error.message);
  }
  return new ViewQueryLocalError(
    fallback,
    error instanceof Error ? error.message : String(error),
  );
}

function requireFetch(candidate: DiagnosticFetch | undefined): DiagnosticFetch {
  if (typeof candidate !== "function") {
    throw new ViewQueryLocalError("transport", "native fetch is unavailable");
  }
  return candidate;
}

function requestKey(
  generation: ViewQueryGeneration,
  pagination: FrozenViewPagination,
): string {
  return JSON.stringify([generation.key, viewPaginationKey(pagination)]);
}

const IDLE_QUERY_STATE: ViewQueryState = {
  status: "idle",
  generation_key: null,
  result: null,
  error: null,
};

export class ViewQueryClient {
  private readonly runId: CanonicalUuid;
  private readonly origin: string;
  private readonly endpoint: URL;
  private readonly fetchImpl: DiagnosticFetch;
  private readonly timeoutMs: number;
  private readonly results = new BoundedViewQueryCache<ViewQueryResult>();
  private readonly queryStates = new Map<string, ViewQueryState>();
  private catalogPromise: Promise<ViewCatalog> | null = null;
  private catalogController: AbortController | null = null;
  private catalogStateValue: ViewCatalogState = {
    status: "idle",
    catalog: null,
    error: null,
  };

  constructor(options: ViewQueryClientOptions) {
    this.runId = decodeCanonicalUuid(options.bootstrap.identity.run_id, "bootstrap.run_id");
    this.origin = options.bootstrap.origin;
    this.endpoint = diagnosticApiUrl(options.bootstrap, "views");
    this.fetchImpl = requireFetch(options.fetch ?? globalThis.fetch);
    const timeout = options.request_timeout_ms ?? DEFAULT_REQUEST_TIMEOUT_MS;
    if (!Number.isSafeInteger(timeout) || timeout < 1) {
      throw new RangeError("view request timeout must be a positive safe integer");
    }
    this.timeoutMs = timeout;
  }

  get catalogState(): ViewCatalogState {
    return this.catalogStateValue;
  }

  get resultCacheSize(): number {
    return this.results.size;
  }

  queryState(viewId: string): ViewQueryState {
    return this.queryStates.get(viewId) ?? IDLE_QUERY_STATE;
  }

  loadCatalog(): Promise<ViewCatalog> {
    if (this.catalogPromise !== null) {
      return this.catalogPromise;
    }
    this.catalogStateValue = { status: "loading", catalog: null, error: null };
    const controller = new AbortController();
    this.catalogController = controller;
    this.catalogPromise = this.fetchJson(this.endpoint, controller.signal, "view catalog")
      .then((value) => {
        const catalog = decodeViewCatalog(value);
        if (catalog.run_id !== this.runId) {
          throw new ViewQueryLocalError("identity", "view catalog belongs to another Run");
        }
        this.catalogStateValue = { status: "ready", catalog, error: null };
        return catalog;
      })
      .catch((raw: unknown) => {
        const error = localizeError(raw, "catalog");
        this.catalogStateValue = { status: "local_error", catalog: null, error };
        throw error;
      })
      .finally(() => {
        this.catalogController = null;
      });
    return this.catalogPromise;
  }

  async query(
    viewId: string,
    context: ViewQueryContext,
    paginationInput: ViewPaginationInput = {},
  ): Promise<ViewQueryResult> {
    let catalog: ViewCatalog;
    try {
      catalog = await this.loadCatalog();
    } catch (raw) {
      const error = localizeError(raw, "catalog");
      this.setLocalError(viewId, null, error, null);
      throw error;
    }
    const entry = catalog.views.find((candidate) => viewCatalogEntryId(candidate) === viewId);
    if (entry === undefined) {
      const error = new ViewQueryLocalError(
        "view_not_found",
        `compiled view ${JSON.stringify(viewId)} is not in the catalog`,
      );
      this.setLocalError(viewId, null, error, null);
      throw error;
    }
    if (!isCompatibleViewCatalogEntry(entry)) {
      const error = new ViewQueryLocalError(
        "incompatible",
        `compiled view ${JSON.stringify(viewId)} is unavailable: ${entry.incompatible.reason}`,
      );
      this.setLocalError(viewId, null, error, null);
      throw error;
    }

    let generation: ViewQueryGeneration;
    try {
      generation = freezeViewQueryGeneration(this.runId, entry, context);
    } catch (raw) {
      this.results.invalidateView(viewId);
      const error = localizeError(raw, "binding");
      this.setLocalError(viewId, null, error, null);
      throw error;
    }
    let pagination: FrozenViewPagination;
    try {
      pagination = freezeViewPagination(entry, catalog.capabilities, paginationInput);
    } catch (raw) {
      this.results.invalidateView(viewId);
      const error = localizeError(raw, "pagination");
      this.setLocalError(viewId, generation.key, error, null);
      throw error;
    }
    return this.execute(entry, generation, pagination);
  }

  async nextPage(result: ViewQueryResult): Promise<ViewQueryResult | null> {
    const pagination = nextViewPagination(result.response);
    if (pagination === null) {
      return null;
    }
    const generation = result.generation;
    if (!this.results.isGenerationActive(generation.view_id, generation.key)) {
      throw new ViewQueryLocalError("stale", "cannot paginate a stale view query result");
    }
    const catalog = await this.loadCatalog();
    const entry = catalog.views.find(
      (candidate) => viewCatalogEntryId(candidate) === generation.view_id,
    );
    if (
      entry === undefined
      || !isCompatibleViewCatalogEntry(entry)
      || entry.renderer !== generation.renderer
    ) {
      throw new ViewQueryLocalError("stale", "catalog entry no longer matches the query result");
    }
    return this.execute(entry, generation, pagination);
  }

  reportRendererFailure(viewId: string, failure: unknown): ViewQueryLocalError {
    const error = new ViewQueryLocalError(
      "renderer",
      failure instanceof Error ? failure.message : String(failure),
    );
    const current = this.queryState(viewId);
    const result = current.status === "ready" || current.status === "local_error"
      ? current.result
      : null;
    const generationKey = current.generation_key;
    this.setLocalError(viewId, generationKey, error, result);
    return error;
  }

  invalidateView(viewId: string): void {
    this.results.invalidateView(viewId);
    this.queryStates.delete(viewId);
  }

  dispose(): void {
    this.catalogController?.abort();
    this.catalogController = null;
    this.results.dispose();
    this.queryStates.clear();
  }

  private async execute(
    record: ViewRecord,
    generation: ViewQueryGeneration,
    pagination: FrozenViewPagination,
  ): Promise<ViewQueryResult> {
    const key = requestKey(generation, pagination);
    const load = this.results.request({
      view_id: generation.view_id,
      generation_key: generation.key,
      request_key: key,
      load: async (signal) => {
        const url = new URL(this.endpoint.href);
        appendViewBindingParameters(url.searchParams, generation);
        appendViewPaginationParameters(url.searchParams, pagination);
        const value = await this.fetchJson(url, signal, `view ${generation.view_id}`);
        const response = decodeViewResponse(value, record);
        assertViewResponseGeneration(response, generation);
        assertViewResponsePagination(response, pagination);
        return {
          generation,
          pagination,
          response,
          time_series: isTimeSeriesResponse(response)
            ? toTimeSeriesColumnarModel(response)
            : null,
        };
      },
    });
    if (load.source !== "cache") {
      this.queryStates.set(generation.view_id, {
        status: "loading",
        generation_key: generation.key,
        result: null,
        error: null,
      });
    }
    try {
      const result = await load.promise;
      if (this.results.isRequestActive(generation.view_id, generation.key, key)) {
        this.queryStates.set(generation.view_id, {
          status: "ready",
          generation_key: generation.key,
          result,
          error: null,
        });
      }
      return result;
    } catch (raw) {
      const error = localizeError(raw, "protocol");
      if (this.results.isRequestActive(generation.view_id, generation.key, key)) {
        this.setLocalError(generation.view_id, generation.key, error, null);
      }
      throw error;
    }
  }

  private setLocalError(
    viewId: string,
    generationKey: string | null,
    error: ViewQueryLocalError,
    result: ViewQueryResult | null,
  ): void {
    this.queryStates.set(viewId, {
      status: "local_error",
      generation_key: generationKey,
      result,
      error,
    });
  }

  private async fetchJson(url: URL, signal: AbortSignal, label: string): Promise<unknown> {
    assertSameOriginUrl(url, this.origin);
    const linked = new AbortController();
    let timedOut = false;
    const abort = () => linked.abort();
    if (signal.aborted) {
      linked.abort();
    } else {
      signal.addEventListener("abort", abort, { once: true });
    }
    const timeout = globalThis.setTimeout(() => {
      timedOut = true;
      linked.abort();
    }, this.timeoutMs);
    let response: Response;
    try {
      response = await this.fetchImpl.call(globalThis, url, {
        method: "GET",
        headers: { Accept: "application/json" },
        cache: "no-store",
        credentials: "same-origin",
        redirect: "error",
        signal: linked.signal,
      });
    } catch (raw) {
      globalThis.clearTimeout(timeout);
      signal.removeEventListener("abort", abort);
      if (timedOut) {
        throw new ViewQueryLocalError("timeout", `${label} request timed out`);
      }
      if (signal.aborted) {
        throw raw;
      }
      throw new ViewQueryLocalError(
        "transport",
        `${label} request failed: ${raw instanceof Error ? raw.message : String(raw)}`,
      );
    }

    let text: string;
    try {
      text = await response.text();
    } catch (raw) {
      if (timedOut) {
        throw new ViewQueryLocalError("timeout", `${label} response timed out`);
      }
      if (signal.aborted) {
        throw raw;
      }
      throw new ViewQueryLocalError(
        "transport",
        `${label} response failed: ${raw instanceof Error ? raw.message : String(raw)}`,
      );
    } finally {
      globalThis.clearTimeout(timeout);
      signal.removeEventListener("abort", abort);
    }
    if (timedOut) {
      throw new ViewQueryLocalError("timeout", `${label} response timed out`);
    }
    const value = parseProtocolJson(text, label);
    if (!response.ok) {
      let message = `${label} returned HTTP ${response.status}`;
      try {
        const decoded = decodeHttpErrorResponse(value, `${label}.error`);
        message = `${message}: ${decoded.error.code}: ${decoded.error.message}`;
      } catch {
        // The HTTP status remains authoritative when the error body is malformed.
      }
      throw new ViewQueryLocalError("http", message, response.status);
    }
    const contentType = response.headers.get("content-type");
    if (contentType !== null && !contentType.toLowerCase().startsWith("application/json")) {
      throw new ViewQueryLocalError("protocol", `${label} did not return JSON`);
    }
    return value;
  }
}

export function createViewQueryClient(options: ViewQueryClientOptions): ViewQueryClient {
  return new ViewQueryClient(options);
}
