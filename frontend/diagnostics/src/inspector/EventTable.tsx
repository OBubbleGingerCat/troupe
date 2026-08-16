import {
  ChevronLeft,
  ChevronRight,
} from "lucide-preact";
import type { JSX } from "preact";

import type { U64String } from "../protocol/decimal.ts";
import type { DiagnosticEvent } from "../protocol/event.ts";
import type { SelectionReference } from "../state/model.ts";
import {
  eventReference,
  sameSelectionReference,
} from "../state/selection.ts";
import {
  eventSelectionHighlight,
  selectionReferenceForEvent,
} from "./selection.ts";
import "./inspector.css";


export interface EventPageCursor {
  readonly after: U64String | null;
}

export interface EventTablePage {
  readonly events: readonly DiagnosticEvent[];
  readonly captured_through: U64String;
  readonly previous: EventPageCursor | null;
  readonly next: EventPageCursor | null;
}

export interface EventPageRequest {
  readonly direction: "previous" | "next";
  readonly cursor: EventPageCursor;
}

export interface EventTableProps {
  readonly page: EventTablePage;
  readonly selection: SelectionReference | null;
  readonly onSelectionChange: (selection: SelectionReference) => void;
  readonly onPageRequest: (request: EventPageRequest) => void;
  readonly selectionEvents?: readonly DiagnosticEvent[];
  readonly loading?: boolean;
}

function optionalValue(value: string | null): string {
  return value ?? "Unknown";
}

export function summarizeEvent(event: DiagnosticEvent): string {
  switch (event.kind) {
    case "span_started":
      return `Started ${event.span_kind}`;
    case "span_finished":
      return `Finished span ${event.span_id}: ${event.outcome}`;
    case "instant_occurred":
      return event.instant_kind;
    case "counter_sampled":
      return `${event.counter_kind}: ${event.value}`;
    case "agent_message_delta":
      return event.text_delta;
    case "agent_message_completed":
      return `Message ${event.message_id} completed${event.truncated ? " (truncated)" : ""}`;
    case "agent_plan_snapshot":
      return `Plan snapshot: ${event.entries.length} entries${event.truncated ? " (truncated)" : ""}`;
    case "context_usage_sampled":
      return [
        "Context",
        optionalValue(event.context_used_tokens),
        "/",
        optionalValue(event.context_window_tokens),
        "tokens",
      ].join(" ");
    case "act_token_usage_finalized":
      return `Act usage ${event.availability}: ${optionalValue(event.provider_total_tokens)} total tokens`;
    case "observation_gap":
      return `Observation gap: ${event.reason}`;
    case "custom_span_started":
      return `Started ${event.name}`;
    case "custom_span_finished":
      return `Finished span ${event.span_id}: ${event.outcome}`;
    case "custom_instant_occurred":
      return event.name;
    case "custom_counter_sampled":
      return `${event.name}: ${event.value.value}${event.unit === null ? "" : ` ${event.unit}`}`;
    default: {
      const exhaustive: never = event;
      return exhaustive;
    }
  }
}

function isIssue(event: DiagnosticEvent): boolean {
  switch (event.kind) {
    case "observation_gap":
      return true;
    case "span_finished":
      return event.outcome === "failed";
    case "custom_span_finished":
      return event.outcome === "failed";
    case "custom_instant_occurred":
      return event.severity === "error" || event.severity === "warning";
    case "instant_occurred":
      return event.instant_kind === "diagnostic.component_failed"
        || event.instant_kind === "agent.session.broken"
        || event.instant_kind === "result.rejected"
        || event.instant_kind === "result.missing";
    default:
      return false;
  }
}

function semanticReference(event: DiagnosticEvent): SelectionReference | null {
  const reference = selectionReferenceForEvent(event);
  return sameSelectionReference(reference, eventReference(event.sequence))
    ? null
    : reference;
}

function pageStatus(page: EventTablePage): string {
  const first = page.events[0];
  const last = page.events[page.events.length - 1];
  if (first === undefined || last === undefined) {
    return `No events; captured through ${page.captured_through}`;
  }
  return `Events ${first.sequence} to ${last.sequence}; captured through ${page.captured_through}`;
}

export function EventTable({
  page,
  selection,
  onSelectionChange,
  onPageRequest,
  selectionEvents = page.events,
  loading = false,
}: EventTableProps): JSX.Element {
  return (
    <section class="diagnostic-event-table" aria-label="Diagnostic events">
      <div class="diagnostic-event-table__scroll" data-testid="event-table-scroll">
        <table>
          <thead>
            <tr>
              <th scope="col">Sequence</th>
              <th scope="col">Elapsed</th>
              <th scope="col">Type</th>
              <th scope="col">Actor</th>
              <th scope="col">Summary</th>
            </tr>
          </thead>
          <tbody>
            {page.events.length === 0 ? (
              <tr>
                <td class="diagnostic-event-table__empty" colSpan={5}>No events match this query.</td>
              </tr>
            ) : page.events.map((event) => {
              const highlight = eventSelectionHighlight(event, selection, selectionEvents);
              const semantic = semanticReference(event);
              return (
                <tr
                  key={`${event.run_id}:${event.sequence}`}
                  data-event-sequence={event.sequence}
                  data-selection={highlight}
                  aria-selected={highlight === "selected"}
                  class={isIssue(event) ? "diagnostic-event-table__issue" : undefined}
                >
                  <td class="diagnostic-event-table__number">
                    <button
                      type="button"
                      class="diagnostic-link-button diagnostic-number"
                      aria-label={`Select event ${event.sequence}`}
                      onClick={() => onSelectionChange(eventReference(event.sequence))}
                    >
                      {event.sequence}
                    </button>
                  </td>
                  <td class="diagnostic-event-table__number">
                    <span class="diagnostic-number">{event.elapsed_ns} ns</span>
                  </td>
                  <td><code>{event.kind}</code></td>
                  <td>{event.scope.actor_id ?? <span class="diagnostic-unknown">Unknown</span>}</td>
                  <td
                    class="diagnostic-event-table__summary diagnostic-long-content"
                    data-testid={`event-summary-${event.sequence}`}
                  >
                    {semantic === null ? summarizeEvent(event) : (
                      <button
                        type="button"
                        class="diagnostic-link-button diagnostic-event-table__semantic-link"
                        onClick={() => onSelectionChange(semantic)}
                      >
                        {summarizeEvent(event)}
                      </button>
                    )}
                  </td>
                </tr>
              );
            })}
          </tbody>
        </table>
      </div>

      <footer class="diagnostic-event-table__pagination" aria-label="Event pages">
        <button
          class="diagnostic-icon-button"
          type="button"
          aria-label="Previous event page"
          title="Previous event page"
          disabled={loading || page.previous === null}
          onClick={() => {
            if (page.previous !== null) {
              onPageRequest({ direction: "previous", cursor: page.previous });
            }
          }}
        >
          <ChevronLeft aria-hidden="true" size={17} strokeWidth={1.75} />
        </button>
        <output aria-live="polite">{loading ? "Loading events" : pageStatus(page)}</output>
        <button
          class="diagnostic-icon-button"
          type="button"
          aria-label="Next event page"
          title="Next event page"
          disabled={loading || page.next === null}
          onClick={() => {
            if (page.next !== null) {
              onPageRequest({ direction: "next", cursor: page.next });
            }
          }}
        >
          <ChevronRight aria-hidden="true" size={17} strokeWidth={1.75} />
        </button>
      </footer>
    </section>
  );
}
