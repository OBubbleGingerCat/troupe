import {
  type CanonicalIntegerString,
  type CanonicalUuid,
  type DecimalString,
  type JsonObject,
  type TokenIntegerString,
  type U64String,
  decodeCanonicalInteger,
  decodeCanonicalUuid,
  decodeDecimal,
  decodeTokenInteger,
  decodeU64,
  expectArray,
  expectBoolean,
  expectEnum,
  expectExactFields,
  expectObject,
  expectString,
  failProtocol,
  parseProtocolJson,
} from "./decimal.ts";


export const DIAGNOSTIC_EVENT_KINDS = [
  "span_started",
  "span_finished",
  "instant_occurred",
  "counter_sampled",
  "agent_message_delta",
  "agent_message_completed",
  "agent_plan_snapshot",
  "context_usage_sampled",
  "act_token_usage_finalized",
  "observation_gap",
  "custom_span_started",
  "custom_span_finished",
  "custom_instant_occurred",
  "custom_counter_sampled",
] as const;

export type DiagnosticEventKind = typeof DIAGNOSTIC_EVENT_KINDS[number];

export const SPAN_KINDS = [
  "run.lifecycle",
  "production.path_resolution",
  "production.load",
  "production.construct",
  "production.start",
  "production.stop",
  "production.shutdown",
  "scene.lifecycle",
  "scene.drain",
  "scene.cleanup",
  "actor.handle_lifetime",
  "cue.mailbox_wait",
  "cue.execution",
  "effect.lifecycle",
  "agent.session.opening",
  "agent.session.lifecycle",
  "agent.session.closing",
  "act.lifecycle",
  "act.caller",
  "agent.turn",
  "agent.thinking",
  "tool.call",
] as const;

export const INSTANT_KINDS = [
  "actor.cast",
  "cue.admitted",
  "cue.enqueued",
  "cue.dispatched",
  "cue.cancel_requested",
  "effect.created",
  "effect.returned",
  "effect.consumed",
  "agent.session.ready",
  "agent.session.broken",
  "act.admitted",
  "act.waiting_ready",
  "act.prompt_submitted",
  "act.cancel_requested",
  "act.supervisor_handoff",
  "agent.turn.activity",
  "agent.turn.terminal",
  "agent.turn.settled",
  "tool.updated",
  "result.submitted",
  "result.rejected",
  "result.repair_requested",
  "result.accepted",
  "result.missing",
  "diagnostic.component_failed",
] as const;

export const COUNTER_KINDS = [
  "actor.mailbox_depth",
  "cue.active",
  "agent.turn.active",
  "result.validation_rejections",
  "diagnostic.dropped_events",
] as const;

const CAUSAL_RELATIONS = ["dispatch", "return", "handoff", "retry", "follows_from"] as const;
const SPAN_OUTCOMES = ["completed", "cancelled", "failed"] as const;
const SEVERITIES = ["debug", "info", "warning", "error"] as const;
const COMMON_FIELDS = [
  "schema_version",
  "run_id",
  "sequence",
  "elapsed_ns",
  "scope",
  "caused_by",
  "kind",
] as const;

const EVENT_FIELDS: Readonly<Record<DiagnosticEventKind, readonly string[]>> = {
  span_started: ["span_kind", "detail", "parent_span_id"],
  span_finished: ["span_id", "outcome", "error_code"],
  instant_occurred: ["instant_kind", "detail", "containing_span_id"],
  counter_sampled: ["counter_kind", "value"],
  agent_message_delta: ["message_id", "source_message_id", "text_delta"],
  agent_message_completed: ["message_id", "utf8_bytes", "unicode_scalar_count", "truncated"],
  agent_plan_snapshot: ["entries", "truncated"],
  context_usage_sampled: [
    "context_used_tokens",
    "context_window_tokens",
    "cumulative_cost_amount",
    "cumulative_cost_currency",
    "sample_origin",
    "observed_elapsed_ns",
  ],
  act_token_usage_finalized: [
    "availability",
    "source",
    "unavailable_reason",
    "provider_total_tokens",
    "input_tokens",
    "output_tokens",
    "thought_tokens",
    "cached_read_tokens",
    "cached_write_tokens",
  ],
  observation_gap: [
    "producer",
    "component",
    "reason",
    "dropped_count",
    "affected_elapsed",
    "affected_kind",
    "affected_scope",
  ],
  custom_span_started: ["name", "parent_span_id", "attributes"],
  custom_span_finished: ["span_id", "outcome"],
  custom_instant_occurred: ["name", "containing_span_id", "severity", "attributes"],
  custom_counter_sampled: ["name", "value", "unit", "dimensions"],
};

export interface DiagnosticScope {
  readonly scene_id: string | null;
  readonly actor_id: string | null;
  readonly cue_id: string | null;
  readonly effect_id: string | null;
  readonly act_id: string | null;
  readonly tool_call_id: string | null;
  readonly session_generation: U64String | null;
}

export interface CausalLink {
  readonly source_sequence: U64String;
  readonly relation: typeof CAUSAL_RELATIONS[number];
}

interface DiagnosticEventBase<K extends DiagnosticEventKind> {
  readonly kind: K;
  readonly schema_version: 1;
  readonly run_id: CanonicalUuid;
  readonly sequence: U64String;
  readonly elapsed_ns: U64String;
  readonly scope: DiagnosticScope;
  readonly caused_by: readonly CausalLink[];
}

export interface SpanStartedEvent extends DiagnosticEventBase<"span_started"> {
  readonly span_kind: typeof SPAN_KINDS[number];
  readonly detail: JsonObject;
  readonly parent_span_id: U64String | null;
}

export interface SpanFinishedEvent extends DiagnosticEventBase<"span_finished"> {
  readonly span_id: U64String;
  readonly outcome: typeof SPAN_OUTCOMES[number];
  readonly error_code: string | null;
}

export interface InstantOccurredEvent extends DiagnosticEventBase<"instant_occurred"> {
  readonly instant_kind: typeof INSTANT_KINDS[number];
  readonly detail: JsonObject;
  readonly containing_span_id: U64String | null;
}

export interface CounterSampledEvent extends DiagnosticEventBase<"counter_sampled"> {
  readonly counter_kind: typeof COUNTER_KINDS[number];
  readonly value: U64String;
}

export interface AgentMessageDeltaEvent extends DiagnosticEventBase<"agent_message_delta"> {
  readonly message_id: string;
  readonly source_message_id: string | null;
  readonly text_delta: string;
}

export interface AgentMessageCompletedEvent extends DiagnosticEventBase<"agent_message_completed"> {
  readonly message_id: string;
  readonly utf8_bytes: U64String;
  readonly unicode_scalar_count: U64String;
  readonly truncated: boolean;
}

export interface AgentPlanSnapshotEvent extends DiagnosticEventBase<"agent_plan_snapshot"> {
  readonly entries: readonly {
    readonly content: string;
    readonly priority: "high" | "medium" | "low";
    readonly status: "pending" | "in_progress" | "completed";
  }[];
  readonly truncated: boolean;
}

export interface ContextUsageSampledEvent extends DiagnosticEventBase<"context_usage_sampled"> {
  readonly context_used_tokens: U64String | null;
  readonly context_window_tokens: U64String | null;
  readonly cumulative_cost_amount: DecimalString | null;
  readonly cumulative_cost_currency: string | null;
  readonly sample_origin: "provider" | "carried_forward";
  readonly observed_elapsed_ns: U64String | null;
}

export interface ActTokenUsageFinalizedEvent
  extends DiagnosticEventBase<"act_token_usage_finalized"> {
  readonly availability: "available" | "partial" | "unavailable";
  readonly source: "acp.prompt_response.usage" | null;
  readonly unavailable_reason:
    | "prompt_not_submitted"
    | "source_unsupported"
    | "usage_not_reported"
    | "turn_settlement_unknown"
    | null;
  readonly provider_total_tokens: TokenIntegerString | null;
  readonly input_tokens: TokenIntegerString | null;
  readonly output_tokens: TokenIntegerString | null;
  readonly thought_tokens: TokenIntegerString | null;
  readonly cached_read_tokens: TokenIntegerString | null;
  readonly cached_write_tokens: TokenIntegerString | null;
}

export interface ObservationGapEvent extends DiagnosticEventBase<"observation_gap"> {
  readonly producer: string;
  readonly component: string | null;
  readonly reason: string;
  readonly dropped_count: U64String | null;
  readonly affected_elapsed: { readonly start_ns: U64String; readonly end_ns: U64String } | null;
  readonly affected_kind: DiagnosticEventKind | null;
  readonly affected_scope: DiagnosticScope | null;
}

export type TaggedScalar =
  | { readonly type: "null" }
  | { readonly type: "boolean"; readonly value: boolean }
  | { readonly type: "integer"; readonly value: CanonicalIntegerString }
  | { readonly type: "decimal"; readonly value: DecimalString }
  | { readonly type: "string"; readonly value: string };

export type TaggedAttributeScalar = TaggedScalar | {
  readonly type: "list";
  readonly value: readonly TaggedScalar[];
};

export interface CustomSpanStartedEvent extends DiagnosticEventBase<"custom_span_started"> {
  readonly name: string;
  readonly parent_span_id: U64String | null;
  readonly attributes: Readonly<Record<string, TaggedAttributeScalar>>;
}

export interface CustomSpanFinishedEvent extends DiagnosticEventBase<"custom_span_finished"> {
  readonly span_id: U64String;
  readonly outcome: typeof SPAN_OUTCOMES[number];
}

export interface CustomInstantOccurredEvent extends DiagnosticEventBase<"custom_instant_occurred"> {
  readonly name: string;
  readonly containing_span_id: U64String | null;
  readonly severity: typeof SEVERITIES[number] | null;
  readonly attributes: Readonly<Record<string, TaggedAttributeScalar>>;
}

export interface CustomCounterSampledEvent extends DiagnosticEventBase<"custom_counter_sampled"> {
  readonly name: string;
  readonly value:
    | { readonly type: "integer"; readonly value: CanonicalIntegerString }
    | { readonly type: "decimal"; readonly value: DecimalString };
  readonly unit: string | null;
  readonly dimensions: Readonly<Record<string, Exclude<TaggedScalar, { readonly type: "null" }>>>;
}

export type DiagnosticEvent =
  | SpanStartedEvent
  | SpanFinishedEvent
  | InstantOccurredEvent
  | CounterSampledEvent
  | AgentMessageDeltaEvent
  | AgentMessageCompletedEvent
  | AgentPlanSnapshotEvent
  | ContextUsageSampledEvent
  | ActTokenUsageFinalizedEvent
  | ObservationGapEvent
  | CustomSpanStartedEvent
  | CustomSpanFinishedEvent
  | CustomInstantOccurredEvent
  | CustomCounterSampledEvent;

function optional<T>(value: unknown, decoder: (item: unknown, path: string) => T, path: string): T | null {
  return value === null ? null : decoder(value, path);
}

function utf8ByteLength(value: string): number {
  let bytes = 0;
  for (const character of value) {
    const point = character.codePointAt(0)!;
    bytes += point <= 0x7f ? 1 : point <= 0x7ff ? 2 : point <= 0xffff ? 3 : 4;
  }
  return bytes;
}

function decodeRunLocalId(value: unknown, path: string): string {
  const text = expectString(value, path);
  if (text.length === 0 || !/^[\x00-\x7f]+$/.test(text) || utf8ByteLength(text) > 128) {
    failProtocol("run_local_id", path, "expected nonempty ASCII with at most 128 bytes");
  }
  return text;
}

export function decodeDiagnosticScope(value: unknown, path = "scope"): DiagnosticScope {
  const scope = expectObject(value, path);
  const fields = [
    "scene_id",
    "actor_id",
    "cue_id",
    "effect_id",
    "act_id",
    "tool_call_id",
    "session_generation",
  ] as const;
  expectExactFields(scope, fields, path);
  for (const field of fields.slice(0, 6)) {
    optional(scope[field], decodeRunLocalId, `${path}.${field}`);
  }
  const generation = optional(scope.session_generation, decodeU64, `${path}.session_generation`);
  if (generation === "0") {
    failProtocol("scope", `${path}.session_generation`, "zero is the unknown sentinel");
  }
  return scope as unknown as DiagnosticScope;
}

function validateCausalLinks(value: unknown, sequence: U64String, path: string): void {
  const links = expectArray(value, path);
  if (links.length > 16) {
    failProtocol("causal", path, "more than 16 links");
  }
  links.forEach((raw, index) => {
    const itemPath = `${path}[${index}]`;
    const link = expectObject(raw, itemPath);
    expectExactFields(link, ["source_sequence", "relation"], itemPath);
    const source = decodeU64(link.source_sequence, `${itemPath}.source_sequence`);
    if (BigInt(source) >= BigInt(sequence)) {
      failProtocol("causal", itemPath, "link is not backward");
    }
    expectEnum(link.relation, CAUSAL_RELATIONS, `${itemPath}.relation`);
  });
}

function emptyDetail(value: unknown, path: string): void {
  expectExactFields(expectObject(value, path), [], path);
}

function actorDetail(value: unknown, path: string): void {
  const detail = expectObject(value, path);
  expectExactFields(detail, ["display_name", "actor_type"], path);
  expectString(detail.display_name, `${path}.display_name`);
  expectString(detail.actor_type, `${path}.actor_type`);
}

function effectDetail(value: unknown, path: string): void {
  const detail = expectObject(value, path);
  expectExactFields(detail, ["effect_type"], path);
  expectString(detail.effect_type, `${path}.effect_type`);
}

function sessionDetail(value: unknown, path: string): void {
  const detail = expectObject(value, path);
  expectExactFields(detail, ["provider", "effective_model", "effective_effort"], path);
  expectString(detail.provider, `${path}.provider`);
  optional(detail.effective_model, expectString, `${path}.effective_model`);
  optional(detail.effective_effort, expectString, `${path}.effective_effort`);
}

function toolDetail(value: unknown, path: string): void {
  const detail = expectObject(value, path);
  expectExactFields(detail, ["title", "tool_kind", "status", "error_code"], path);
  expectString(detail.title, `${path}.title`);
  expectEnum(
    detail.tool_kind,
    ["read", "edit", "delete", "move", "search", "execute", "think", "fetch", "switch_mode", "other"],
    `${path}.tool_kind`,
  );
  expectEnum(detail.status, ["pending", "in_progress", "completed", "failed"], `${path}.status`);
  optional(detail.error_code, expectString, `${path}.error_code`);
}

function resultDetail(value: unknown, path: string): void {
  const detail = expectObject(value, path);
  expectExactFields(detail, ["issue", "error_code"], path);
  if (detail.issue !== null) {
    const issue = expectObject(detail.issue, `${path}.issue`);
    expectExactFields(issue, ["code", "path"], `${path}.issue`);
    expectString(issue.code, `${path}.issue.code`);
    expectString(issue.path, `${path}.issue.path`);
  }
  optional(detail.error_code, expectString, `${path}.error_code`);
}

function componentFailureDetail(value: unknown, path: string): void {
  const detail = expectObject(value, path);
  expectExactFields(
    detail,
    ["component", "component_id", "stage", "error_code", "related_event_sequence"],
    path,
  );
  if (detail.component !== "sink") {
    failProtocol("component_failure", `${path}.component`, "component must be sink");
  }
  decodeRunLocalId(detail.component_id, `${path}.component_id`);
  const stage = expectEnum(detail.stage, ["enqueue", "callback"], `${path}.stage`);
  const error = expectEnum(
    detail.error_code,
    ["delivery_queue_unavailable", "callback_raised", "callback_invalid_return"],
    `${path}.error_code`,
  );
  const valid = stage === "enqueue"
    ? error === "delivery_queue_unavailable"
    : error === "callback_raised" || error === "callback_invalid_return";
  if (!valid) {
    failProtocol("component_failure", path, "stage and error code do not match");
  }
  optional(detail.related_event_sequence, decodeU64, `${path}.related_event_sequence`);
}

function validateSpanDetail(kind: typeof SPAN_KINDS[number], value: unknown, path: string): void {
  if ([
    "run.lifecycle",
    "production.start",
    "production.stop",
    "production.shutdown",
    "scene.lifecycle",
    "scene.drain",
    "scene.cleanup",
    "cue.mailbox_wait",
    "cue.execution",
    "act.caller",
    "agent.thinking",
  ].includes(kind)) {
    emptyDetail(value, path);
  } else if (kind === "production.path_resolution") {
    const detail = expectObject(value, path);
    expectExactFields(detail, ["production_root", "package"], path);
    expectString(detail.production_root, `${path}.production_root`);
    expectString(detail.package, `${path}.package`);
  } else if (kind === "production.load") {
    const detail = expectObject(value, path);
    expectExactFields(detail, ["package"], path);
    expectString(detail.package, `${path}.package`);
  } else if (kind === "production.construct") {
    const detail = expectObject(value, path);
    expectExactFields(detail, ["package", "class_name"], path);
    expectString(detail.package, `${path}.package`);
    expectString(detail.class_name, `${path}.class_name`);
  } else if (kind === "actor.handle_lifetime") {
    actorDetail(value, path);
  } else if (kind === "effect.lifecycle") {
    effectDetail(value, path);
  } else if ([
    "agent.session.opening",
    "agent.session.lifecycle",
    "agent.session.closing",
    "act.lifecycle",
    "agent.turn",
  ].includes(kind)) {
    sessionDetail(value, path);
  } else if (kind === "tool.call") {
    toolDetail(value, path);
  }
}

function validateInstantDetail(kind: typeof INSTANT_KINDS[number], value: unknown, path: string): void {
  if (kind === "actor.cast") {
    actorDetail(value, path);
  } else if ([
    "cue.admitted",
    "cue.enqueued",
    "cue.dispatched",
    "cue.cancel_requested",
    "act.admitted",
    "act.waiting_ready",
    "act.prompt_submitted",
    "act.cancel_requested",
    "act.supervisor_handoff",
    "agent.turn.activity",
  ].includes(kind)) {
    emptyDetail(value, path);
  } else if (["effect.created", "effect.returned", "effect.consumed"].includes(kind)) {
    effectDetail(value, path);
  } else if (kind === "agent.session.ready") {
    sessionDetail(value, path);
  } else if (kind === "agent.session.broken") {
    const detail = expectObject(value, path);
    expectExactFields(detail, ["provider", "effective_model", "effective_effort", "error_code"], path);
    expectString(detail.provider, `${path}.provider`);
    optional(detail.effective_model, expectString, `${path}.effective_model`);
    optional(detail.effective_effort, expectString, `${path}.effective_effort`);
    expectString(detail.error_code, `${path}.error_code`);
  } else if (kind === "agent.turn.terminal" || kind === "agent.turn.settled") {
    const detail = expectObject(value, path);
    expectExactFields(detail, ["error_code"], path);
    optional(detail.error_code, expectString, `${path}.error_code`);
  } else if (kind === "tool.updated") {
    toolDetail(value, path);
  } else if ([
    "result.submitted",
    "result.rejected",
    "result.repair_requested",
    "result.accepted",
    "result.missing",
  ].includes(kind)) {
    resultDetail(value, path);
  } else if (kind === "diagnostic.component_failed") {
    componentFailureDetail(value, path);
  }
}

function decodeCustomName(value: unknown, path: string): string {
  const name = expectString(value, path);
  const segments = name.split(".");
  if (
    name.length === 0
    || !/^[\x00-\x7f]+$/.test(name)
    || utf8ByteLength(name) > 128
    || segments.length < 2
    || segments[0] === "troupe"
    || segments.some((segment) => !/^[a-z][a-z0-9_]*$/.test(segment))
  ) {
    failProtocol("custom_name", path, "name is invalid, reserved, or out of bounds");
  }
  return name;
}

function validateCustomKey(value: string, path: string): void {
  if (value.length === 0 || utf8ByteLength(value) > 64) {
    failProtocol("custom_key", path, "key is out of bounds");
  }
}

function validateTaggedScalar(
  value: unknown,
  path: string,
  options: { attribute: boolean; dimension?: boolean },
): void {
  const tagged = expectObject(value, path);
  const allowed = options.attribute
    ? ["null", "boolean", "integer", "decimal", "string", "list"] as const
    : options.dimension
      ? ["boolean", "integer", "decimal", "string"] as const
      : ["null", "boolean", "integer", "decimal", "string"] as const;
  const kind = expectEnum(tagged.type, allowed, `${path}.type`);
  expectExactFields(tagged, kind === "null" ? ["type"] : ["type", "value"], path);
  if (kind === "null") {
    return;
  }
  if (kind === "boolean") {
    expectBoolean(tagged.value, `${path}.value`);
  } else if (kind === "integer") {
    decodeCanonicalInteger(tagged.value, `${path}.value`);
  } else if (kind === "decimal") {
    decodeDecimal(tagged.value, `${path}.value`);
  } else if (kind === "string") {
    expectString(tagged.value, `${path}.value`);
  } else {
    const items = expectArray(tagged.value, `${path}.value`);
    if (items.length > 64) {
      failProtocol("custom_list", `${path}.value`, "list is too long");
    }
    items.forEach((item, index) => validateTaggedScalar(
      item,
      `${path}.value[${index}]`,
      { attribute: false },
    ));
  }
}

function validateScalarMap(value: unknown, path: string, dimension: boolean): void {
  const object = expectObject(value, path);
  const keys = Object.keys(object);
  const maximum = dimension ? 8 : 32;
  if (keys.length > maximum) {
    failProtocol(dimension ? "custom_dimensions" : "custom_attributes", path, "too many entries");
  }
  if (keys.join("\0") !== [...keys].sort().join("\0")) {
    failProtocol("custom_order", path, "keys are not in canonical order");
  }
  for (const key of keys) {
    validateCustomKey(key, `${path}.<key>`);
    validateTaggedScalar(object[key], `${path}.${key}`, {
      attribute: !dimension,
      dimension,
    });
  }
}

function validateCustomNumber(value: unknown, path: string): void {
  const number = expectObject(value, path);
  expectExactFields(number, ["type", "value"], path);
  const kind = expectEnum(number.type, ["integer", "decimal"], `${path}.type`);
  (kind === "integer" ? decodeCanonicalInteger : decodeDecimal)(number.value, `${path}.value`);
}

function validateEventVariant(event: Record<string, unknown>, kind: DiagnosticEventKind, path: string): void {
  switch (kind) {
    case "span_started": {
      const spanKind = expectEnum(event.span_kind, SPAN_KINDS, `${path}.span_kind`);
      validateSpanDetail(spanKind, event.detail, `${path}.detail`);
      optional(event.parent_span_id, decodeU64, `${path}.parent_span_id`);
      break;
    }
    case "span_finished":
      decodeU64(event.span_id, `${path}.span_id`);
      expectEnum(event.outcome, SPAN_OUTCOMES, `${path}.outcome`);
      optional(event.error_code, expectString, `${path}.error_code`);
      break;
    case "instant_occurred": {
      const instantKind = expectEnum(event.instant_kind, INSTANT_KINDS, `${path}.instant_kind`);
      validateInstantDetail(instantKind, event.detail, `${path}.detail`);
      optional(event.containing_span_id, decodeU64, `${path}.containing_span_id`);
      break;
    }
    case "counter_sampled":
      expectEnum(event.counter_kind, COUNTER_KINDS, `${path}.counter_kind`);
      decodeU64(event.value, `${path}.value`);
      break;
    case "agent_message_delta":
      decodeRunLocalId(event.message_id, `${path}.message_id`);
      optional(event.source_message_id, expectString, `${path}.source_message_id`);
      expectString(event.text_delta, `${path}.text_delta`);
      break;
    case "agent_message_completed":
      decodeRunLocalId(event.message_id, `${path}.message_id`);
      decodeU64(event.utf8_bytes, `${path}.utf8_bytes`);
      decodeU64(event.unicode_scalar_count, `${path}.unicode_scalar_count`);
      expectBoolean(event.truncated, `${path}.truncated`);
      break;
    case "agent_plan_snapshot":
      expectArray(event.entries, `${path}.entries`).forEach((raw, index) => {
        const entryPath = `${path}.entries[${index}]`;
        const entry = expectObject(raw, entryPath);
        expectExactFields(entry, ["content", "priority", "status"], entryPath);
        expectString(entry.content, `${entryPath}.content`);
        expectEnum(entry.priority, ["high", "medium", "low"], `${entryPath}.priority`);
        expectEnum(entry.status, ["pending", "in_progress", "completed"], `${entryPath}.status`);
      });
      expectBoolean(event.truncated, `${path}.truncated`);
      break;
    case "context_usage_sampled": {
      const used = optional(event.context_used_tokens, decodeU64, `${path}.context_used_tokens`);
      const window = optional(event.context_window_tokens, decodeU64, `${path}.context_window_tokens`);
      const amount = optional(event.cumulative_cost_amount, decodeDecimal, `${path}.cumulative_cost_amount`);
      const currency = optional(event.cumulative_cost_currency, (value, itemPath) => {
        const text = expectString(value, itemPath);
        if (!/^[A-Z]{3}$/.test(text)) {
          failProtocol("currency", itemPath, "expected three uppercase ASCII letters");
        }
        return text;
      }, `${path}.cumulative_cost_currency`);
      const origin = expectEnum(event.sample_origin, ["provider", "carried_forward"], `${path}.sample_origin`);
      const observed = optional(event.observed_elapsed_ns, decodeU64, `${path}.observed_elapsed_ns`);
      if ((amount === null) !== (currency === null)) {
        failProtocol("optional", path, "cost amount and currency must appear together");
      }
      if (amount !== null && amount.startsWith("-")) {
        failProtocol("decimal", `${path}.cumulative_cost_amount`, "cost must be nonnegative");
      }
      if (used !== null && window !== null && BigInt(used) > BigInt(window)) {
        failProtocol("context_usage", path, "used tokens exceed window");
      }
      if (origin === "carried_forward" && observed === null) {
        failProtocol("optional", `${path}.observed_elapsed_ns`, "carried sample needs observation time");
      }
      break;
    }
    case "act_token_usage_finalized": {
      const availability = expectEnum(
        event.availability,
        ["available", "partial", "unavailable"],
        `${path}.availability`,
      );
      const source = optional(
        event.source,
        (value, itemPath) => expectEnum(value, ["acp.prompt_response.usage"], itemPath),
        `${path}.source`,
      );
      const reason = optional(
        event.unavailable_reason,
        (value, itemPath) => expectEnum(
          value,
          ["prompt_not_submitted", "source_unsupported", "usage_not_reported", "turn_settlement_unknown"],
          itemPath,
        ),
        `${path}.unavailable_reason`,
      );
      const tokenFields = [
        "provider_total_tokens",
        "input_tokens",
        "output_tokens",
        "thought_tokens",
        "cached_read_tokens",
        "cached_write_tokens",
      ] as const;
      const values = tokenFields.map((field) => optional(event[field], decodeTokenInteger, `${path}.${field}`));
      const primaryComplete = values.slice(0, 3).every((value) => value !== null);
      const anyValue = values.some((value) => value !== null);
      const valid = availability === "available"
        ? primaryComplete && source !== null && reason === null
        : availability === "partial"
          ? anyValue && !primaryComplete && source !== null && reason === null
          : !anyValue && source === null && reason !== null;
      if (!valid) {
        failProtocol("usage", path, "availability fields are inconsistent");
      }
      break;
    }
    case "observation_gap":
      expectString(event.producer, `${path}.producer`);
      optional(event.component, expectString, `${path}.component`);
      expectString(event.reason, `${path}.reason`);
      optional(event.dropped_count, decodeU64, `${path}.dropped_count`);
      if (event.affected_elapsed !== null) {
        const interval = expectObject(event.affected_elapsed, `${path}.affected_elapsed`);
        expectExactFields(interval, ["start_ns", "end_ns"], `${path}.affected_elapsed`);
        decodeU64(interval.start_ns, `${path}.affected_elapsed.start_ns`);
        decodeU64(interval.end_ns, `${path}.affected_elapsed.end_ns`);
      }
      if (event.affected_kind !== null) {
        expectEnum(event.affected_kind, DIAGNOSTIC_EVENT_KINDS, `${path}.affected_kind`);
      }
      if (event.affected_scope !== null) {
        decodeDiagnosticScope(event.affected_scope, `${path}.affected_scope`);
      }
      break;
    case "custom_span_started":
      decodeCustomName(event.name, `${path}.name`);
      optional(event.parent_span_id, decodeU64, `${path}.parent_span_id`);
      validateScalarMap(event.attributes, `${path}.attributes`, false);
      break;
    case "custom_span_finished":
      decodeU64(event.span_id, `${path}.span_id`);
      expectEnum(event.outcome, SPAN_OUTCOMES, `${path}.outcome`);
      break;
    case "custom_instant_occurred":
      decodeCustomName(event.name, `${path}.name`);
      optional(event.containing_span_id, decodeU64, `${path}.containing_span_id`);
      optional(
        event.severity,
        (value, itemPath) => expectEnum(value, SEVERITIES, itemPath),
        `${path}.severity`,
      );
      validateScalarMap(event.attributes, `${path}.attributes`, false);
      break;
    case "custom_counter_sampled": {
      decodeCustomName(event.name, `${path}.name`);
      validateCustomNumber(event.value, `${path}.value`);
      const unit = optional(event.unit, expectString, `${path}.unit`);
      if (unit !== null && (unit.length === 0 || utf8ByteLength(unit) > 32)) {
        failProtocol("custom_unit", `${path}.unit`, "unit is out of bounds");
      }
      validateScalarMap(event.dimensions, `${path}.dimensions`, true);
      break;
    }
  }
}

export function decodeDiagnosticEvent(value: unknown, path = "event"): DiagnosticEvent {
  const event = expectObject(value, path);
  const kind = expectEnum(event.kind, DIAGNOSTIC_EVENT_KINDS, `${path}.kind`);
  expectExactFields(event, [...COMMON_FIELDS, ...EVENT_FIELDS[kind]], path);
  if (event.schema_version !== 1) {
    failProtocol("schema_version", `${path}.schema_version`, "expected integer 1");
  }
  decodeCanonicalUuid(event.run_id, `${path}.run_id`);
  const sequence = decodeU64(event.sequence, `${path}.sequence`);
  if (sequence === "0") {
    failProtocol("u64", `${path}.sequence`, "sequence must start at one");
  }
  decodeU64(event.elapsed_ns, `${path}.elapsed_ns`);
  decodeDiagnosticScope(event.scope, `${path}.scope`);
  validateCausalLinks(event.caused_by, sequence, `${path}.caused_by`);
  validateEventVariant(event, kind, path);
  return event as unknown as DiagnosticEvent;
}

export function decodeDiagnosticEventJson(text: string): DiagnosticEvent {
  return decodeDiagnosticEvent(parseProtocolJson(text, "event_json"));
}

export function encodeDiagnosticEventJson(event: DiagnosticEvent): string {
  return JSON.stringify(event);
}
