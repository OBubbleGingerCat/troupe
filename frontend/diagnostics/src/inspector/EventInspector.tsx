import { AlertTriangle } from "lucide-preact";
import type {
  ComponentChildren,
  JSX,
} from "preact";

import type {
  JsonObject,
  JsonValue,
  U64String,
} from "../protocol/decimal.ts";
import type {
  DiagnosticEvent,
  DiagnosticScope,
  TaggedAttributeScalar,
  TaggedScalar,
} from "../protocol/event.ts";
import type { SelectionReference } from "../state/model.ts";
import {
  eventSelectionReference,
  messageSelectionReference,
  scopeFieldSelectionReference,
  spanSelectionReference,
} from "./selection.ts";
import "./inspector.css";


export interface EventInspectorProps {
  readonly event: DiagnosticEvent | null;
  readonly onSelectionChange?: (selection: SelectionReference) => void;
}

interface DetailRowProps {
  readonly label: string;
  readonly children: ComponentChildren;
}

interface ReferenceValueProps {
  readonly value: string | null;
  readonly reference: SelectionReference | null;
  readonly onSelectionChange: ((selection: SelectionReference) => void) | undefined;
  readonly numeric?: boolean;
}

function Unknown(): JSX.Element {
  return <span class="diagnostic-unknown">Unknown</span>;
}

function NumberText({ value }: { readonly value: string | null }): JSX.Element {
  return value === null
    ? <Unknown />
    : <code class="diagnostic-number">{value}</code>;
}

function DetailRow({ label, children }: DetailRowProps): JSX.Element {
  return (
    <div class="diagnostic-detail-row">
      <dt>{label}</dt>
      <dd>{children}</dd>
    </div>
  );
}

function ReferenceValue({
  value,
  reference,
  onSelectionChange,
  numeric = false,
}: ReferenceValueProps): JSX.Element {
  if (value === null) {
    return <Unknown />;
  }
  if (reference === null || onSelectionChange === undefined) {
    return numeric
      ? <code class="diagnostic-number">{value}</code>
      : <span class="diagnostic-text-value">{value}</span>;
  }
  return (
    <button
      type="button"
      class={`diagnostic-link-button${numeric ? " diagnostic-number" : ""}`}
      onClick={() => onSelectionChange(reference)}
    >
      {value}
    </button>
  );
}

function JsonValueView({ value }: { readonly value: JsonValue }): JSX.Element {
  if (value === null) {
    return <Unknown />;
  }
  if (typeof value === "string") {
    return <span class="diagnostic-text-value">{value}</span>;
  }
  if (typeof value === "number") {
    return <code class="diagnostic-number">{String(value)}</code>;
  }
  if (typeof value === "boolean") {
    return <span>{value ? "True" : "False"}</span>;
  }
  if (Array.isArray(value)) {
    return value.length === 0 ? <span>Empty list</span> : (
      <ol class="diagnostic-value-list">
        {value.map((item, index) => (
          <li key={index}><JsonValueView value={item} /></li>
        ))}
      </ol>
    );
  }
  return <JsonObjectView value={value as JsonObject} />;
}

function JsonObjectView({ value }: { readonly value: JsonObject }): JSX.Element {
  const entries = Object.entries(value);
  if (entries.length === 0) {
    return <span>No detail</span>;
  }
  return (
    <dl class="diagnostic-inline-detail">
      {entries.map(([key, item]) => (
        <DetailRow key={key} label={key}><JsonValueView value={item} /></DetailRow>
      ))}
    </dl>
  );
}

function TaggedValueView({
  value,
}: {
  readonly value: TaggedAttributeScalar | TaggedScalar;
}): JSX.Element {
  if (value.type === "null") {
    return <Unknown />;
  }
  if (value.type === "list") {
    return value.value.length === 0 ? <span>Empty list</span> : (
      <ol class="diagnostic-value-list">
        {value.value.map((item, index) => (
          <li key={index}><TaggedValueView value={item} /></li>
        ))}
      </ol>
    );
  }
  if (value.type === "boolean") {
    return <span>{value.value ? "True" : "False"}</span>;
  }
  if (value.type === "integer" || value.type === "decimal") {
    return <code class="diagnostic-number">{value.value}</code>;
  }
  return <span class="diagnostic-text-value">{value.value}</span>;
}

function AttributeMap({
  attributes,
}: {
  readonly attributes: Readonly<Record<string, TaggedAttributeScalar | TaggedScalar>>;
}): JSX.Element {
  const entries = Object.entries(attributes);
  return entries.length === 0 ? <span>No attributes</span> : (
    <dl class="diagnostic-inline-detail">
      {entries.map(([key, value]) => (
        <DetailRow key={key} label={key}><TaggedValueView value={value} /></DetailRow>
      ))}
    </dl>
  );
}

function ScopeDetails({
  scope,
  onSelectionChange,
}: {
  readonly scope: DiagnosticScope;
  readonly onSelectionChange: ((selection: SelectionReference) => void) | undefined;
}): JSX.Element {
  return (
    <dl class="diagnostic-detail-list">
      <DetailRow label="Scene">
        <ReferenceValue
          value={scope.scene_id}
          reference={scope.scene_id === null
            ? null
            : scopeFieldSelectionReference("scene_id", scope.scene_id)}
          onSelectionChange={onSelectionChange}
        />
      </DetailRow>
      <DetailRow label="Actor">
        <ReferenceValue
          value={scope.actor_id}
          reference={scope.actor_id === null
            ? null
            : scopeFieldSelectionReference("actor_id", scope.actor_id)}
          onSelectionChange={onSelectionChange}
        />
      </DetailRow>
      <DetailRow label="Cue">
        <ReferenceValue
          value={scope.cue_id}
          reference={scope.cue_id === null
            ? null
            : scopeFieldSelectionReference("cue_id", scope.cue_id)}
          onSelectionChange={onSelectionChange}
        />
      </DetailRow>
      <DetailRow label="Effect">
        <ReferenceValue
          value={scope.effect_id}
          reference={scope.effect_id === null
            ? null
            : scopeFieldSelectionReference("effect_id", scope.effect_id)}
          onSelectionChange={onSelectionChange}
        />
      </DetailRow>
      <DetailRow label="Act">
        <ReferenceValue
          value={scope.act_id}
          reference={scope.act_id === null
            ? null
            : scopeFieldSelectionReference("act_id", scope.act_id)}
          onSelectionChange={onSelectionChange}
        />
      </DetailRow>
      <DetailRow label="Tool call">
        <ReferenceValue
          value={scope.tool_call_id}
          reference={scope.tool_call_id === null
            ? null
            : scopeFieldSelectionReference("tool_call_id", scope.tool_call_id)}
          onSelectionChange={onSelectionChange}
        />
      </DetailRow>
      <DetailRow label="Session generation">
        <NumberText value={scope.session_generation} />
      </DetailRow>
    </dl>
  );
}

function SpanReference({
  value,
  onSelectionChange,
}: {
  readonly value: U64String | null;
  readonly onSelectionChange: ((selection: SelectionReference) => void) | undefined;
}): JSX.Element {
  return (
    <ReferenceValue
      value={value}
      reference={value === null ? null : spanSelectionReference(value)}
      onSelectionChange={onSelectionChange}
      numeric
    />
  );
}

function TruncationStatus({ truncated }: { readonly truncated: boolean }): JSX.Element {
  return truncated ? (
    <span class="diagnostic-status diagnostic-status--warning">Yes, content is incomplete</span>
  ) : <span>No</span>;
}

function EventDetail({
  event,
  onSelectionChange,
}: {
  readonly event: DiagnosticEvent;
  readonly onSelectionChange: ((selection: SelectionReference) => void) | undefined;
}): JSX.Element {
  switch (event.kind) {
    case "span_started":
      return (
        <dl class="diagnostic-detail-list">
          <DetailRow label="Span kind"><code>{event.span_kind}</code></DetailRow>
          <DetailRow label="Parent span">
            <SpanReference
              value={event.parent_span_id}
              onSelectionChange={onSelectionChange}
            />
          </DetailRow>
          <DetailRow label="Detail"><JsonObjectView value={event.detail} /></DetailRow>
        </dl>
      );
    case "span_finished":
      return (
        <dl class="diagnostic-detail-list">
          <DetailRow label="Span">
            <SpanReference value={event.span_id} onSelectionChange={onSelectionChange} />
          </DetailRow>
          <DetailRow label="Outcome">
            <span class={`diagnostic-status diagnostic-status--${event.outcome}`}>
              {event.outcome}
            </span>
          </DetailRow>
          <DetailRow label="Error code">{event.error_code ?? <Unknown />}</DetailRow>
        </dl>
      );
    case "instant_occurred":
      return (
        <dl class="diagnostic-detail-list">
          <DetailRow label="Instant kind"><code>{event.instant_kind}</code></DetailRow>
          <DetailRow label="Containing span">
            <SpanReference
              value={event.containing_span_id}
              onSelectionChange={onSelectionChange}
            />
          </DetailRow>
          <DetailRow label="Detail"><JsonObjectView value={event.detail} /></DetailRow>
        </dl>
      );
    case "counter_sampled":
      return (
        <dl class="diagnostic-detail-list">
          <DetailRow label="Counter kind"><code>{event.counter_kind}</code></DetailRow>
          <DetailRow label="Value"><NumberText value={event.value} /></DetailRow>
        </dl>
      );
    case "agent_message_delta":
      return (
        <dl class="diagnostic-detail-list">
          <DetailRow label="Message">
            <ReferenceValue
              value={event.message_id}
              reference={messageSelectionReference(event.message_id)}
              onSelectionChange={onSelectionChange}
            />
          </DetailRow>
          <DetailRow label="Source message">
            <ReferenceValue
              value={event.source_message_id}
              reference={event.source_message_id === null
                ? null
                : messageSelectionReference(event.source_message_id)}
              onSelectionChange={onSelectionChange}
            />
          </DetailRow>
          <DetailRow label="Delta">
            <pre class="diagnostic-message-text diagnostic-long-content">{event.text_delta}</pre>
          </DetailRow>
        </dl>
      );
    case "agent_message_completed":
      return (
        <dl class="diagnostic-detail-list">
          <DetailRow label="Message">
            <ReferenceValue
              value={event.message_id}
              reference={messageSelectionReference(event.message_id)}
              onSelectionChange={onSelectionChange}
            />
          </DetailRow>
          <DetailRow label="UTF-8 bytes"><NumberText value={event.utf8_bytes} /></DetailRow>
          <DetailRow label="Unicode scalars"><NumberText value={event.unicode_scalar_count} /></DetailRow>
          <DetailRow label="Truncated"><TruncationStatus truncated={event.truncated} /></DetailRow>
        </dl>
      );
    case "agent_plan_snapshot":
      return (
        <div class="diagnostic-plan-detail">
          <dl class="diagnostic-detail-list">
            <DetailRow label="Truncated"><TruncationStatus truncated={event.truncated} /></DetailRow>
          </dl>
          {event.entries.length === 0 ? <p>No plan entries.</p> : (
            <ol class="diagnostic-plan-entries">
              {event.entries.map((entry, index) => (
                <li key={index}>
                  <span class="diagnostic-plan-entries__meta">{entry.status} / {entry.priority}</span>
                  <span class="diagnostic-text-value">{entry.content}</span>
                </li>
              ))}
            </ol>
          )}
        </div>
      );
    case "context_usage_sampled":
      return (
        <dl class="diagnostic-detail-list">
          <DetailRow label="Context used tokens"><NumberText value={event.context_used_tokens} /></DetailRow>
          <DetailRow label="Context window tokens"><NumberText value={event.context_window_tokens} /></DetailRow>
          <DetailRow label="Cumulative cost"><NumberText value={event.cumulative_cost_amount} /></DetailRow>
          <DetailRow label="Cost currency">{event.cumulative_cost_currency ?? <Unknown />}</DetailRow>
          <DetailRow label="Sample origin"><code>{event.sample_origin}</code></DetailRow>
          <DetailRow label="Observed elapsed"><NumberText value={event.observed_elapsed_ns} /></DetailRow>
        </dl>
      );
    case "act_token_usage_finalized":
      return (
        <dl class="diagnostic-detail-list">
          <DetailRow label="Availability"><span class="diagnostic-status">{event.availability}</span></DetailRow>
          <DetailRow label="Source">{event.source ?? <Unknown />}</DetailRow>
          <DetailRow label="Unavailable reason">{event.unavailable_reason ?? <Unknown />}</DetailRow>
          <DetailRow label="Provider total tokens"><NumberText value={event.provider_total_tokens} /></DetailRow>
          <DetailRow label="Input tokens"><NumberText value={event.input_tokens} /></DetailRow>
          <DetailRow label="Output tokens"><NumberText value={event.output_tokens} /></DetailRow>
          <DetailRow label="Thought tokens"><NumberText value={event.thought_tokens} /></DetailRow>
          <DetailRow label="Cached read tokens"><NumberText value={event.cached_read_tokens} /></DetailRow>
          <DetailRow label="Cached write tokens"><NumberText value={event.cached_write_tokens} /></DetailRow>
        </dl>
      );
    case "observation_gap":
      return (
        <div class="diagnostic-gap-detail">
          <p class="diagnostic-gap-detail__notice">
            <AlertTriangle aria-hidden="true" size={17} strokeWidth={1.75} />
            Some observations are unavailable for this interval.
          </p>
          <dl class="diagnostic-detail-list">
            <DetailRow label="Producer">{event.producer}</DetailRow>
            <DetailRow label="Component">{event.component ?? <Unknown />}</DetailRow>
            <DetailRow label="Reason">{event.reason}</DetailRow>
            <DetailRow label="Dropped count"><NumberText value={event.dropped_count} /></DetailRow>
            <DetailRow label="Affected elapsed">
              {event.affected_elapsed === null ? <Unknown /> : (
                <span class="diagnostic-number">
                  {event.affected_elapsed.start_ns} to {event.affected_elapsed.end_ns} ns
                </span>
              )}
            </DetailRow>
            <DetailRow label="Affected event kind">{event.affected_kind ?? <Unknown />}</DetailRow>
            <DetailRow label="Affected scope">
              {event.affected_scope === null
                ? <Unknown />
                : <ScopeDetails scope={event.affected_scope} onSelectionChange={onSelectionChange} />}
            </DetailRow>
          </dl>
        </div>
      );
    case "custom_span_started":
      return (
        <dl class="diagnostic-detail-list">
          <DetailRow label="Name">{event.name}</DetailRow>
          <DetailRow label="Parent span">
            <SpanReference
              value={event.parent_span_id}
              onSelectionChange={onSelectionChange}
            />
          </DetailRow>
          <DetailRow label="Attributes"><AttributeMap attributes={event.attributes} /></DetailRow>
        </dl>
      );
    case "custom_span_finished":
      return (
        <dl class="diagnostic-detail-list">
          <DetailRow label="Span">
            <SpanReference value={event.span_id} onSelectionChange={onSelectionChange} />
          </DetailRow>
          <DetailRow label="Outcome">
            <span class={`diagnostic-status diagnostic-status--${event.outcome}`}>
              {event.outcome}
            </span>
          </DetailRow>
        </dl>
      );
    case "custom_instant_occurred":
      return (
        <dl class="diagnostic-detail-list">
          <DetailRow label="Name">{event.name}</DetailRow>
          <DetailRow label="Containing span">
            <SpanReference
              value={event.containing_span_id}
              onSelectionChange={onSelectionChange}
            />
          </DetailRow>
          <DetailRow label="Severity">{event.severity ?? <Unknown />}</DetailRow>
          <DetailRow label="Attributes"><AttributeMap attributes={event.attributes} /></DetailRow>
        </dl>
      );
    case "custom_counter_sampled":
      return (
        <dl class="diagnostic-detail-list">
          <DetailRow label="Name">{event.name}</DetailRow>
          <DetailRow label="Value"><TaggedValueView value={event.value} /></DetailRow>
          <DetailRow label="Unit">{event.unit ?? <Unknown />}</DetailRow>
          <DetailRow label="Dimensions"><AttributeMap attributes={event.dimensions} /></DetailRow>
        </dl>
      );
    default: {
      const exhaustive: never = event;
      return exhaustive;
    }
  }
}

export function EventInspector({
  event,
  onSelectionChange,
}: EventInspectorProps): JSX.Element {
  if (event === null) {
    return (
      <aside class="diagnostic-event-inspector diagnostic-event-inspector--empty" aria-label="Event inspector">
        <p>Select an event to inspect its typed diagnostic detail.</p>
      </aside>
    );
  }

  return (
    <aside class="diagnostic-event-inspector" aria-label="Event inspector">
      <header class="diagnostic-event-inspector__header">
        <div>
          <p class="diagnostic-event-inspector__eyebrow">Event</p>
          <h2>{event.kind}</h2>
        </div>
        <ReferenceValue
          value={event.sequence}
          reference={eventSelectionReference(event.sequence)}
          onSelectionChange={onSelectionChange}
          numeric
        />
      </header>

      <section class="diagnostic-event-inspector__section" aria-labelledby="event-identity-heading">
        <h3 id="event-identity-heading">Identity</h3>
        <dl class="diagnostic-detail-list">
          <DetailRow label="Run"><code>{event.run_id}</code></DetailRow>
          <DetailRow label="Sequence"><NumberText value={event.sequence} /></DetailRow>
          <DetailRow label="Elapsed"><span class="diagnostic-number">{event.elapsed_ns} ns</span></DetailRow>
        </dl>
      </section>

      <section class="diagnostic-event-inspector__section" aria-labelledby="event-scope-heading">
        <h3 id="event-scope-heading">Scope</h3>
        <ScopeDetails scope={event.scope} onSelectionChange={onSelectionChange} />
      </section>

      <section class="diagnostic-event-inspector__section" aria-labelledby="event-causal-heading">
        <h3 id="event-causal-heading">Causal links</h3>
        {event.caused_by.length === 0 ? <p>None</p> : (
          <ul class="diagnostic-causal-links">
            {event.caused_by.map((link) => (
              <li key={`${link.source_sequence}:${link.relation}`}>
                <ReferenceValue
                  value={link.source_sequence}
                  reference={eventSelectionReference(link.source_sequence)}
                  onSelectionChange={onSelectionChange}
                  numeric
                />
                <span>{link.relation}</span>
              </li>
            ))}
          </ul>
        )}
      </section>

      <section class="diagnostic-event-inspector__section" aria-labelledby="event-detail-heading">
        <h3 id="event-detail-heading">Typed detail</h3>
        <EventDetail event={event} onSelectionChange={onSelectionChange} />
      </section>
    </aside>
  );
}
