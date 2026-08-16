import {
  createFixedLru,
  lruDelete,
  lruGet,
  lruSet,
  type FixedLru,
} from "../state/lru.ts";
import { QUERY_RESULT_CAPACITY } from "../state/model.ts";


interface CachedQueryValue<T> {
  readonly view_id: string;
  readonly generation_key: string;
  readonly value: T;
}

interface ActiveQuery {
  readonly generation_key: string;
  readonly request_key: string;
}

interface InflightQuery<T> extends ActiveQuery {
  readonly view_id: string;
  readonly controller: AbortController;
  readonly promise: Promise<T>;
}

export interface QueryCacheRequest<T> {
  readonly view_id: string;
  readonly generation_key: string;
  readonly request_key: string;
  readonly load: (signal: AbortSignal) => Promise<T>;
}

export interface QueryCacheLoad<T> {
  readonly source: "cache" | "inflight" | "network";
  readonly promise: Promise<T>;
}

export class StaleViewQueryError extends Error {
  constructor() {
    super("view query result belongs to a stale generation");
    this.name = "StaleViewQueryError";
  }
}

export class BoundedViewQueryCache<T> {
  private cache: FixedLru<string, CachedQueryValue<T>>;
  private readonly active = new Map<string, ActiveQuery>();
  private readonly inflight = new Map<string, InflightQuery<T>>();

  constructor(capacity = QUERY_RESULT_CAPACITY) {
    this.cache = createFixedLru(capacity);
  }

  get size(): number {
    return this.cache.entries.size;
  }

  request(request: QueryCacheRequest<T>): QueryCacheLoad<T> {
    const previous = this.active.get(request.view_id);
    if (previous?.generation_key !== request.generation_key) {
      this.abortInflightForView(request.view_id);
      this.deleteCachedForView(request.view_id);
    } else if (previous.request_key !== request.request_key) {
      this.abortInflightForView(request.view_id);
    }
    this.active.set(request.view_id, {
      generation_key: request.generation_key,
      request_key: request.request_key,
    });

    const pending = this.inflight.get(request.request_key);
    if (
      pending !== undefined
      && pending.view_id === request.view_id
      && pending.generation_key === request.generation_key
    ) {
      return { source: "inflight", promise: pending.promise };
    }

    const cached = lruGet(this.cache, request.request_key);
    this.cache = cached.state;
    if (
      cached.value !== undefined
      && cached.value.view_id === request.view_id
      && cached.value.generation_key === request.generation_key
    ) {
      return { source: "cache", promise: Promise.resolve(cached.value.value) };
    }

    const controller = new AbortController();
    let promise: Promise<T>;
    promise = Promise.resolve()
      .then(() => request.load(controller.signal))
      .then((value) => {
        if (!this.isRequestActive(request.view_id, request.generation_key, request.request_key)) {
          throw new StaleViewQueryError();
        }
        this.cache = lruSet(this.cache, request.request_key, {
          view_id: request.view_id,
          generation_key: request.generation_key,
          value,
        }).state;
        return value;
      })
      .catch((error: unknown) => {
        if (!this.isRequestActive(request.view_id, request.generation_key, request.request_key)) {
          throw new StaleViewQueryError();
        }
        throw error;
      })
      .finally(() => {
        if (this.inflight.get(request.request_key)?.promise === promise) {
          this.inflight.delete(request.request_key);
        }
      });
    this.inflight.set(request.request_key, {
      view_id: request.view_id,
      generation_key: request.generation_key,
      request_key: request.request_key,
      controller,
      promise,
    });
    return { source: "network", promise };
  }

  isGenerationActive(viewId: string, generationKey: string): boolean {
    return this.active.get(viewId)?.generation_key === generationKey;
  }

  isRequestActive(viewId: string, generationKey: string, requestKey: string): boolean {
    const active = this.active.get(viewId);
    return active?.generation_key === generationKey && active.request_key === requestKey;
  }

  invalidateView(viewId: string): void {
    this.abortInflightForView(viewId);
    this.deleteCachedForView(viewId);
    this.active.delete(viewId);
  }

  dispose(): void {
    for (const pending of this.inflight.values()) {
      pending.controller.abort();
    }
    this.inflight.clear();
    this.active.clear();
    this.cache = createFixedLru(this.cache.capacity);
  }

  private abortInflightForView(viewId: string): void {
    for (const [key, pending] of this.inflight) {
      if (pending.view_id === viewId) {
        this.inflight.delete(key);
        pending.controller.abort();
      }
    }
  }

  private deleteCachedForView(viewId: string): void {
    for (const [key, cached] of this.cache.entries) {
      if (cached.view_id === viewId) {
        this.cache = lruDelete(this.cache, key);
      }
    }
  }
}
