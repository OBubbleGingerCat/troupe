import { compareU64 } from "../protocol/decimal.ts";
import type { DiagnosticEvent, DiagnosticScope, ObservationGapEvent } from "../protocol/event.ts";
import { createFixedLru, lruSet } from "./lru.ts";
import type { CachedQueryResult, QueryCache, QueryDependency } from "./model.ts";
import { QUERY_RESULT_CAPACITY } from "./model.ts";


const SCOPE_FIELDS = [
  "scene_id",
  "actor_id",
  "cue_id",
  "effect_id",
  "act_id",
  "tool_call_id",
  "session_generation",
] as const;

export function createQueryCache(): QueryCache {
  return createFixedLru(QUERY_RESULT_CAPACITY);
}

export function cacheQueryResult(
  cache: QueryCache,
  result: CachedQueryResult,
): QueryCache {
  const previous = cache.entries.get(result.key);
  const unresolvedInvalidation = previous?.invalidated_through !== null
    && previous?.invalidated_through !== undefined
    && compareU64(previous.invalidated_through, result.captured_through) > 0;
  const previousInvalidation = previous?.invalidated_through ?? null;
  const laterInvalidation = previousInvalidation !== null
    && (result.invalidated_through === null
      || compareU64(previousInvalidation, result.invalidated_through) > 0)
    ? previousInvalidation
    : result.invalidated_through;
  const next = unresolvedInvalidation
    ? {
      ...result,
      stale: true,
      invalidated_through: laterInvalidation,
    }
    : result;
  return lruSet(cache, result.key, next).state;
}

function scopeMatches(queryScope: DiagnosticScope, eventScope: DiagnosticScope): boolean {
  return SCOPE_FIELDS.every((field) => {
    const expected = queryScope[field];
    return expected === null || expected === eventScope[field];
  });
}

function kindMatches(dependency: QueryDependency, event: DiagnosticEvent): boolean {
  if (dependency.event_kinds === null) {
    return true;
  }
  if (event.kind !== "observation_gap") {
    return dependency.event_kinds.includes(event.kind);
  }
  return dependency.event_kinds.includes("observation_gap")
    || event.affected_kind === null
    || dependency.event_kinds.includes(event.affected_kind);
}

function rangeIntersectsGap(
  range: NonNullable<QueryDependency["elapsed_range"]>,
  gap: ObservationGapEvent,
): boolean {
  if (gap.affected_elapsed === null) {
    return true;
  }
  return compareU64(gap.affected_elapsed.end_ns, range.start_ns) >= 0
    && compareU64(gap.affected_elapsed.start_ns, range.end_ns) <= 0;
}

export function queryDependsOnEvent(
  dependency: QueryDependency,
  event: DiagnosticEvent,
): boolean {
  if (!kindMatches(dependency, event)) {
    return false;
  }
  const eventScope = event.kind === "observation_gap" && event.affected_scope !== null
    ? event.affected_scope
    : event.scope;
  if (dependency.scope !== null && !scopeMatches(dependency.scope, eventScope)) {
    return false;
  }
  if (dependency.elapsed_range === null) {
    return true;
  }
  if (event.kind === "observation_gap") {
    return rangeIntersectsGap(dependency.elapsed_range, event);
  }
  if (event.kind === "span_finished" || event.kind === "custom_span_finished") {
    return true;
  }
  return compareU64(event.elapsed_ns, dependency.elapsed_range.start_ns) >= 0
    && compareU64(event.elapsed_ns, dependency.elapsed_range.end_ns) <= 0;
}

export function invalidateQueries(cache: QueryCache, event: DiagnosticEvent): QueryCache {
  let changed = false;
  const entries = new Map(cache.entries);
  for (const [key, result] of entries) {
    if (
      compareU64(event.sequence, result.captured_through) > 0
      && queryDependsOnEvent(result.dependency, event)
    ) {
      changed = true;
      entries.set(key, {
        ...result,
        stale: true,
        invalidated_through: result.invalidated_through === null
          || compareU64(event.sequence, result.invalidated_through) > 0
          ? event.sequence
          : result.invalidated_through,
      });
    }
  }
  return changed ? { ...cache, entries } : cache;
}

export function invalidateAllQueries(
  cache: QueryCache,
  throughSequence: CachedQueryResult["captured_through"],
): QueryCache {
  let changed = false;
  const entries = new Map(cache.entries);
  for (const [key, result] of entries) {
    if (compareU64(throughSequence, result.captured_through) > 0) {
      changed = true;
      entries.set(key, {
        ...result,
        stale: true,
        invalidated_through: result.invalidated_through === null
          || compareU64(throughSequence, result.invalidated_through) > 0
          ? throughSequence
          : result.invalidated_through,
      });
    }
  }
  return changed ? { ...cache, entries } : cache;
}
