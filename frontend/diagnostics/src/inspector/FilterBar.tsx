import { X } from "lucide-preact";
import type { JSX } from "preact";

import {
  DIAGNOSTIC_EVENT_KINDS,
  type DiagnosticEventKind,
} from "../protocol/event.ts";
import {
  EMPTY_EVENT_QUERY,
  hasActiveEventQuery,
  type EventErrorFilter,
  type EventQueryState,
} from "./selection.ts";
import "./inspector.css";


export { EMPTY_EVENT_QUERY, hasActiveEventQuery } from "./selection.ts";
export type { EventErrorFilter, EventQueryState } from "./selection.ts";

export interface ActorFilterOption {
  readonly id: string;
  readonly label: string;
}

export interface FilterBarProps {
  readonly query: EventQueryState;
  readonly actors: readonly ActorFilterOption[];
  readonly onQueryChange: (query: EventQueryState) => void;
  readonly disabled?: boolean;
}

function displayEventKind(kind: DiagnosticEventKind): string {
  return kind.split("_").map((part) => (
    part.length === 0 ? part : `${part[0]!.toUpperCase()}${part.slice(1)}`
  )).join(" ");
}

export function FilterBar({
  query,
  actors,
  onQueryChange,
  disabled = false,
}: FilterBarProps): JSX.Element {
  const onActorChange = (event: JSX.TargetedEvent<HTMLSelectElement, Event>): void => {
    onQueryChange({
      ...query,
      actor_id: event.currentTarget.value === "" ? null : event.currentTarget.value,
    });
  };

  const onKindsChange = (event: JSX.TargetedEvent<HTMLSelectElement, Event>): void => {
    const eventKinds = Array.from(
      event.currentTarget.selectedOptions,
      (option) => option.value as DiagnosticEventKind,
    );
    onQueryChange({ ...query, event_kinds: eventKinds });
  };

  const onErrorChange = (event: JSX.TargetedEvent<HTMLSelectElement, Event>): void => {
    onQueryChange({
      ...query,
      error_filter: event.currentTarget.value as EventErrorFilter,
    });
  };

  return (
    <form
      class="diagnostic-filter-bar"
      aria-label="Event query filters"
      onSubmit={(event) => event.preventDefault()}
    >
      <label class="diagnostic-filter-bar__field">
        <span>Actor</span>
        <select
          aria-label="Actor filter"
          value={query.actor_id ?? ""}
          onChange={onActorChange}
          disabled={disabled}
        >
          <option value="">All actors</option>
          {actors.map((actor) => (
            <option key={actor.id} value={actor.id}>{actor.label}</option>
          ))}
        </select>
      </label>

      <label class="diagnostic-filter-bar__field diagnostic-filter-bar__field--kinds">
        <span>Event types</span>
        <select
          multiple
          size={4}
          aria-label="Event type filters"
          onChange={onKindsChange}
          disabled={disabled}
        >
          {DIAGNOSTIC_EVENT_KINDS.map((kind) => (
            <option key={kind} value={kind} selected={query.event_kinds.includes(kind)}>
              {displayEventKind(kind)}
            </option>
          ))}
        </select>
      </label>

      <label class="diagnostic-filter-bar__field">
        <span>Issues</span>
        <select
          aria-label="Error filter"
          value={query.error_filter}
          onChange={onErrorChange}
          disabled={disabled}
        >
          <option value="all">All events</option>
          <option value="errors_only">Errors only</option>
          <option value="errors_and_gaps">Errors and gaps</option>
        </select>
      </label>

      <button
        class="diagnostic-icon-button diagnostic-filter-bar__clear"
        type="button"
        aria-label="Clear event filters"
        title="Clear event filters"
        disabled={disabled || !hasActiveEventQuery(query)}
        onClick={() => onQueryChange(EMPTY_EVENT_QUERY)}
      >
        <X aria-hidden="true" size={16} strokeWidth={1.75} />
      </button>
    </form>
  );
}
