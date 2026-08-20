import { decodeU64 } from "../../src/protocol/decimal.ts";
import {
  decodeDiagnosticEvent,
  type DiagnosticEvent,
  type DiagnosticScope,
} from "../../src/protocol/event.ts";


export const COMPLEX_RUN_ID = "12345678-1234-4234-9234-987654321abc";

function scope(
  sceneId: string | null,
  actorId: string | null = null,
  cueId: string | null = null,
  actId: string | null = null,
  toolCallId: string | null = null,
): DiagnosticScope {
  return {
    scene_id: sceneId,
    actor_id: actorId,
    cue_id: cueId,
    effect_id: null,
    act_id: actId,
    tool_call_id: toolCallId,
    session_generation: actorId === null ? null : decodeU64("1"),
  };
}

const rawEvents: unknown[] = [];
let nextSequence = 1;

function push(at: number, eventScope: DiagnosticScope, fields: Readonly<Record<string, unknown>>): string {
  const sequence = String(nextSequence);
  nextSequence += 1;
  rawEvents.push({
    schema_version: 1,
    run_id: COMPLEX_RUN_ID,
    sequence,
    elapsed_ns: String(Math.round(at * 1_000_000_000)),
    scope: eventScope,
    caused_by: [],
    ...fields,
  });
  return sequence;
}

function startSpan(
  at: number,
  eventScope: DiagnosticScope,
  spanKind: string,
  detail: Readonly<Record<string, unknown>>,
  parentSpanId: string | null = null,
): string {
  return push(at, eventScope, {
    kind: "span_started",
    span_kind: spanKind,
    detail,
    parent_span_id: parentSpanId,
  });
}

function finishSpan(
  at: number,
  eventScope: DiagnosticScope,
  spanId: string,
  outcome: "completed" | "failed" | "cancelled" = "completed",
): void {
  push(at, eventScope, {
    kind: "span_finished",
    span_id: spanId,
    outcome,
    error_code: null,
  });
}

function instant(
  at: number,
  eventScope: DiagnosticScope,
  instantKind: string,
  detail: Readonly<Record<string, unknown>> = {},
  containingSpanId: string | null = null,
): void {
  push(at, eventScope, {
    kind: "instant_occurred",
    instant_kind: instantKind,
    detail,
    containing_span_id: containingSpanId,
  });
}

function customSpan(
  at: number,
  eventScope: DiagnosticScope,
  name: string,
  attributes: Readonly<Record<string, unknown>>,
  parentSpanId: string | null = null,
): string {
  return push(at, eventScope, {
    kind: "custom_span_started",
    name,
    parent_span_id: parentSpanId,
    attributes,
  });
}

function finishCustomSpan(at: number, eventScope: DiagnosticScope, spanId: string): void {
  push(at, eventScope, {
    kind: "custom_span_finished",
    span_id: spanId,
    outcome: "completed",
  });
}

function customEvent(
  at: number,
  eventScope: DiagnosticScope,
  name: string,
  attributes: Readonly<Record<string, unknown>>,
  containingSpanId: string | null,
  severity: "debug" | "info" | "warning" | "error" = "info",
): void {
  push(at, eventScope, {
    kind: "custom_instant_occurred",
    name,
    containing_span_id: containingSpanId,
    severity,
    attributes,
  });
}

function addAct(
  sceneNumber: number,
  actorId: string,
  cueId: string,
  cueStart: number,
  executionStart: number,
  executionEnd: number,
  label: string,
): void {
  const actId = `act-${sceneNumber}-${label}`;
  const actScope = scope(`scene-${sceneNumber}`, actorId, cueId, actId);
  const cueScope = scope(`scene-${sceneNumber}`, actorId, cueId);
  const actStart = executionStart + 0.2;
  const actEnd = executionEnd - 0.25;
  const actSpan = startSpan(
    actStart,
    actScope,
    "act.lifecycle",
    {
      provider: "fixture",
      effective_model: "timeline-fixture",
      effective_effort: "medium",
    },
  );
  const outer = customSpan(
    actStart + 0.15,
    actScope,
    "example.pipeline_cycle",
    { scene: { type: "integer", value: String(sceneNumber) } },
    actSpan,
  );
  const middle = customSpan(
    actStart + 0.35,
    actScope,
    "example.stage_decode",
    { stage: { type: "string", value: label } },
    outer,
  );
  const inner = customSpan(
    actStart + 0.55,
    actScope,
    "example.operation_commit",
    { actor: { type: "string", value: actorId } },
    middle,
  );
  customEvent(
    actStart + 0.65,
    actScope,
    "example.operation_ready",
    { depth: { type: "integer", value: "3" } },
    inner,
  );
  finishCustomSpan(actStart + 0.9, actScope, inner);
  customEvent(
    actStart + 1.05,
    actScope,
    "example.stage_ready",
    { scene: { type: "integer", value: String(sceneNumber) } },
    middle,
  );
  finishCustomSpan(actStart + 1.35, actScope, middle);
  const toolId = `tool-${sceneNumber}-${label}`;
  const toolScope = scope(`scene-${sceneNumber}`, actorId, cueId, actId, toolId);
  const toolSpan = startSpan(
    actStart + 1.45,
    toolScope,
    "tool.call",
    {
      title: `fixture ${label}`,
      tool_kind: "execute",
      status: "completed",
      error_code: null,
    },
    actSpan,
  );
  finishSpan(actStart + 1.7, toolScope, toolSpan);
  finishCustomSpan(actEnd - 0.1, actScope, outer);
  customEvent(
    actEnd - 0.05,
    actScope,
    "example.act_observed",
    { scene: { type: "integer", value: String(sceneNumber) } },
    null,
  );
  push(actEnd - 0.05, actScope, {
    kind: "agent_message_delta",
    message_id: `message-${sceneNumber}-${label}`,
    source_message_id: null,
    text_delta: `fixture ${label} completed`,
  });
  push(actEnd, actScope, {
    kind: "agent_message_completed",
    message_id: `message-${sceneNumber}-${label}`,
    utf8_bytes: String(`fixture ${label} completed`.length),
    unicode_scalar_count: String(`fixture ${label} completed`.length),
    truncated: false,
  });
  instant(actEnd + 0.05, cueScope, "result.accepted", { issue: null, error_code: null }, actSpan);
  finishSpan(actEnd + 0.1, actScope, actSpan);
  void cueStart;
}

function addCue(
  sceneNumber: number,
  actorId: string,
  label: string,
  admitted: number,
  waitEnd: number,
  executionStart: number,
  executionEnd: number,
): void {
  const sceneId = `scene-${sceneNumber}`;
  const cueId = `cue-${sceneNumber}-${label}`;
  const cueScope = scope(sceneId, actorId, cueId);
  instant(admitted, cueScope, "cue.admitted");
  const wait = startSpan(admitted, cueScope, "cue.mailbox_wait", {});
  finishSpan(waitEnd, cueScope, wait);
  const execution = startSpan(executionStart, cueScope, "cue.execution", {});
  addAct(sceneNumber, actorId, cueId, admitted, executionStart, executionEnd, label);
  finishSpan(executionEnd, cueScope, execution);
}

const sceneCount = 48;
const sceneDuration = 15;
const persistentActors = [
  ["actor-ingest", "Ingest worker", "IngestActor"],
  ["actor-review", "Review worker", "ReviewActor"],
  ["actor-publish", "Publish worker", "PublishActor"],
] as const;

for (const [actorId, displayName, actorType] of persistentActors) {
  const actorScope = scope(null, actorId);
  startSpan(
    0.2,
    actorScope,
    "actor.handle_lifetime",
    { display_name: displayName, actor_type: actorType },
  );
  instant(0.25, actorScope, "actor.cast", {
    display_name: displayName,
    actor_type: actorType,
  });
}

for (let sceneNumber = 1; sceneNumber <= sceneCount; sceneNumber += 1) {
  const base = (sceneNumber - 1) * sceneDuration;
  const sceneId = `scene-${sceneNumber}`;
  const sceneScope = scope(sceneId);
  const sceneSpan = startSpan(base, sceneScope, "scene.lifecycle", {});
  const dynamicId = `actor-dynamic-${sceneNumber}`;
  const dynamicScope = scope(sceneId, dynamicId);
  const dynamicLifetime = startSpan(
    base + 0.5,
    dynamicScope,
    "actor.handle_lifetime",
    { display_name: `Dynamic ${sceneNumber}`, actor_type: "EphemeralActor" },
  );
  instant(base + 0.55, dynamicScope, "actor.cast", {
    display_name: `Dynamic ${sceneNumber}`,
    actor_type: "EphemeralActor",
  });

  addCue(sceneNumber, "actor-ingest", "ingest-primary", base + 1, base + 2, base + 2, base + 6);
  addCue(sceneNumber, "actor-ingest", "ingest-followup", base + 1.4, base + 6, base + 6, base + 9);
  addCue(sceneNumber, "actor-review", "review", base + 3, base + 3.8, base + 3.8, base + 9.5);
  addCue(sceneNumber, "actor-publish", "publish", base + 2.2, base + 3.1, base + 3.1, base + 7.5);
  addCue(sceneNumber, dynamicId, "ephemeral", base + 1.8, base + 2.2, base + 2.2, base + 5.2);

  finishSpan(base + 11, dynamicScope, dynamicLifetime);
  instant(base + 11.1, sceneScope, "result.accepted", { issue: null, error_code: null });
  finishSpan(base + 12, sceneScope, sceneSpan);
}

export const COMPLEX_EVENTS: readonly DiagnosticEvent[] = rawEvents.map((event) => (
  decodeDiagnosticEvent(event)
));

export const COMPLEX_WATERMARK = String(COMPLEX_EVENTS.length);
