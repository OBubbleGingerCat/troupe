import type {
  ActTokenUsageFinalizedEvent,
  AgentMessageCompletedEvent,
  ContextUsageSampledEvent,
  CounterSampledEvent,
  CustomCounterSampledEvent,
  CustomSpanFinishedEvent,
  CustomSpanStartedEvent,
  DiagnosticEvent,
  DiagnosticEventKind,
  DiagnosticScope,
  ObservationGapEvent,
  SpanFinishedEvent,
  SpanStartedEvent,
} from "../protocol/event.ts";
import type { CanonicalUuid, U64String } from "../protocol/decimal.ts";
import type { FixedLru } from "./lru.ts";


export const ADJACENT_WINDOW_CAPACITY = 4;
export const VISIBLE_WINDOW_EVENT_CAPACITY = 4_096;
export const LIVE_EDGE_EVENT_CAPACITY = 256;
export const SPAN_CAPACITY = 256;
export const MESSAGE_CAPACITY = 128;
export const MESSAGE_TEXT_CODE_UNIT_CAPACITY = 1_048_576;
export const COUNTER_SERIES_CAPACITY = 256;
export const CONTEXT_USAGE_CAPACITY = 128;
export const ACT_USAGE_CAPACITY = 256;
export const TOOL_FACT_CAPACITY = 256;
export const RESULT_FACT_CAPACITY = 256;
export const GAP_CAPACITY = 128;
export const QUERY_RESULT_CAPACITY = 64;
export const EXPANDED_ITEM_CAPACITY = 128;

export interface SequenceCursor {
  readonly delivered_through: U64String;
  readonly committed_watermark: U64String;
}

export type DeliveryIssue =
  | {
    readonly kind: "cross_run";
    readonly expected_run_id: CanonicalUuid;
    readonly received_run_id: CanonicalUuid;
    readonly received_sequence: U64String;
  }
  | {
    readonly kind: "non_contiguous";
    readonly expected_sequence: U64String;
    readonly received_sequence: U64String;
  };

export interface EventWindow {
  readonly id: string;
  readonly run_id: CanonicalUuid;
  readonly start_ns: U64String;
  readonly end_ns: U64String;
  readonly captured_through: U64String;
  readonly events: readonly DiagnosticEvent[];
}

export interface WindowState {
  readonly visible: EventWindow | null;
  readonly adjacent: FixedLru<string, EventWindow>;
}

export interface ProjectionBucket<T> {
  readonly base_through: U64String;
  readonly items: readonly T[];
  readonly dropped_through: U64String | null;
  readonly needs_server_refresh: boolean;
}

export type SpanStartEvent = SpanStartedEvent | CustomSpanStartedEvent;
export type SpanFinishEvent = SpanFinishedEvent | CustomSpanFinishedEvent;

export interface ProjectedSpan {
  readonly span_id: U64String;
  readonly start: SpanStartEvent | null;
  readonly finish: SpanFinishEvent | null;
}

export interface ProjectedMessage {
  readonly message_id: string;
  readonly scope: DiagnosticScope;
  readonly first_sequence: U64String;
  readonly latest_sequence: U64String;
  readonly latest_elapsed_ns: U64String;
  readonly source_message_id: string | null;
  readonly text: string;
  readonly text_complete_from_start: boolean;
  readonly text_truncated_before: boolean;
  readonly completion: AgentMessageCompletedEvent | null;
}

export interface ProjectedCounter {
  readonly series_key: string;
  readonly event: CounterSampledEvent | CustomCounterSampledEvent;
}

export interface ProjectedContextUsage {
  readonly scope_key: string;
  readonly event: ContextUsageSampledEvent;
}

export interface ProjectedActUsage {
  readonly act_key: string;
  readonly event: ActTokenUsageFinalizedEvent;
}

export type ProjectedToolKind =
  | "read"
  | "edit"
  | "delete"
  | "move"
  | "search"
  | "execute"
  | "think"
  | "fetch"
  | "switch_mode"
  | "other";

export type ProjectedToolStatus = "pending" | "in_progress" | "completed" | "failed";

export interface ProjectedToolFact {
  readonly phase: "started" | "updated" | "finished";
  readonly sequence: U64String;
  readonly elapsed_ns: U64String;
  readonly scope: DiagnosticScope;
  readonly tool_call_id: string | null;
  readonly span_id: U64String | null;
  readonly title: string | null;
  readonly tool_kind: ProjectedToolKind | null;
  readonly status: ProjectedToolStatus | null;
  readonly outcome: SpanFinishedEvent["outcome"] | null;
  readonly error_code: string | null;
}

export type ProjectedResultKind =
  | "result.submitted"
  | "result.rejected"
  | "result.repair_requested"
  | "result.accepted"
  | "result.missing";

export interface ProjectedResultIssue {
  readonly code: string;
  readonly path: string;
}

export interface ProjectedResultFact {
  readonly result_kind: ProjectedResultKind;
  readonly sequence: U64String;
  readonly elapsed_ns: U64String;
  readonly scope: DiagnosticScope;
  readonly act_id: string | null;
  readonly containing_span_id: U64String | null;
  readonly issue: ProjectedResultIssue | null;
  readonly error_code: string | null;
}

export interface GapProjection extends ProjectionBucket<ObservationGapEvent> {
  readonly declared_dropped_count: bigint;
  readonly has_unknown_dropped_count: boolean;
}

export interface LiveProjection {
  readonly spans: ProjectionBucket<ProjectedSpan>;
  readonly messages: ProjectionBucket<ProjectedMessage>;
  readonly counters: ProjectionBucket<ProjectedCounter>;
  readonly context_usage: ProjectionBucket<ProjectedContextUsage>;
  readonly act_usage: ProjectionBucket<ProjectedActUsage>;
  readonly tools: ProjectionBucket<ProjectedToolFact>;
  readonly results: ProjectionBucket<ProjectedResultFact>;
  readonly gaps: GapProjection;
}

export interface LiveEdgeState {
  readonly base_through: U64String;
  readonly observed_elapsed_ns: U64String;
  readonly events: readonly DiagnosticEvent[];
  readonly dropped_through: U64String | null;
  readonly projection: LiveProjection;
}

export interface QueryDependency {
  readonly event_kinds: readonly DiagnosticEventKind[] | null;
  readonly scope: DiagnosticScope | null;
  readonly elapsed_range: {
    readonly start_ns: U64String;
    readonly end_ns: U64String;
  } | null;
}

export interface CachedQueryResult {
  readonly key: string;
  readonly captured_through: U64String;
  readonly value: unknown;
  readonly stale: boolean;
  readonly invalidated_through: U64String | null;
  readonly dependency: QueryDependency;
}

export type QueryCache = FixedLru<string, CachedQueryResult>;

export type SelectionReference = {
  readonly kind: "event" | "span" | "message" | "scope";
  readonly id: string;
};

export interface PresentationFilters {
  readonly event_kinds: readonly DiagnosticEventKind[];
  readonly scene_id: string | null;
  readonly actor_id: string | null;
  readonly text: string;
}

export interface PresentationState {
  readonly selection: SelectionReference | null;
  readonly pinned_detail: SelectionReference | null;
  readonly expanded: readonly string[];
  readonly filters: PresentationFilters;
  readonly viewport: {
    readonly start_ns: U64String;
    readonly end_ns: U64String;
  } | null;
  readonly follow_live: boolean;
  readonly zoom: {
    readonly anchor_ns: U64String;
    readonly scale: number;
  } | null;
}

export interface ServerRangeResumeRequest {
  readonly kind: "server_range";
  readonly after_sequence: U64String;
  readonly through_sequence: U64String;
}

export interface PauseState {
  readonly paused: boolean;
  readonly paused_at: U64String | null;
  readonly unseen_count: bigint;
  readonly resume_request: ServerRangeResumeRequest | null;
  readonly frozen_live: LiveEdgeState | null;
}

export interface DiagnosticState {
  readonly run_id: CanonicalUuid;
  readonly cursor: SequenceCursor;
  readonly delivery_issue: DeliveryIssue | null;
  readonly windows: WindowState;
  readonly live: LiveEdgeState;
  readonly queries: QueryCache;
  readonly presentation: PresentationState;
  readonly pause: PauseState;
}
