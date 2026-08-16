import { AlertTriangle } from "lucide-preact";
import type { JSX } from "preact";

import type { U64String } from "../protocol/decimal.ts";
import type { DiagnosticScope } from "../protocol/event.ts";
import type {
  DiagnosticState,
  ProjectedMessage,
  SelectionReference,
} from "../state/model.ts";
import { presentedLiveEdge } from "../state/reducer.ts";
import {
  MessageStreamItem,
} from "./MessageStream.tsx";
import {
  ToolResultRow,
  type ToolResultItem,
  selectToolResultItems,
  toolResultElapsed,
  toolResultKey,
  toolResultScope,
  toolResultSequence,
} from "./ToolResultRows.tsx";
import "./transcript.css";


export interface TranscriptPanelProps {
  readonly state: DiagnosticState;
  readonly onSelectionChange?: ((selection: SelectionReference) => void) | undefined;
}

type TranscriptEntry =
  | { readonly kind: "message"; readonly message: ProjectedMessage }
  | { readonly kind: "activity"; readonly item: ToolResultItem };

interface TranscriptGroup {
  readonly key: string;
  readonly scope: DiagnosticScope;
  readonly firstSequence: U64String;
  readonly entries: readonly TranscriptEntry[];
}

export interface TranscriptModel {
  readonly groups: readonly TranscriptGroup[];
  readonly observedElapsedNs: U64String;
  readonly messagesIncomplete: boolean;
  readonly activityIncomplete: boolean;
}

function entryScope(entry: TranscriptEntry): DiagnosticScope {
  return entry.kind === "message" ? entry.message.scope : toolResultScope(entry.item);
}

function entrySequence(entry: TranscriptEntry): U64String {
  return entry.kind === "message" ? entry.message.first_sequence : toolResultSequence(entry.item);
}

function entryElapsed(entry: TranscriptEntry): U64String {
  return entry.kind === "message"
    ? entry.message.latest_elapsed_ns
    : toolResultElapsed(entry.item);
}

function groupScope(scope: DiagnosticScope): DiagnosticScope {
  return {
    scene_id: scope.scene_id,
    actor_id: scope.actor_id,
    cue_id: scope.cue_id,
    effect_id: null,
    act_id: scope.act_id,
    tool_call_id: null,
    session_generation: scope.session_generation,
  };
}

function groupKey(scope: DiagnosticScope): string {
  return JSON.stringify([
    scope.scene_id,
    scope.actor_id,
    scope.cue_id,
    scope.act_id,
    scope.session_generation,
  ]);
}

function compareSequence(left: U64String, right: U64String): number {
  const leftValue = BigInt(left);
  const rightValue = BigInt(right);
  return leftValue < rightValue ? -1 : leftValue > rightValue ? 1 : 0;
}

export function selectTranscriptModel(state: DiagnosticState): TranscriptModel {
  const edge = presentedLiveEdge(state);
  const activities = selectToolResultItems(
    edge.projection.spans.items,
    edge.projection.tools.items,
    edge.projection.results.items,
  );
  const entries: TranscriptEntry[] = [
    ...edge.projection.messages.items.map((message): TranscriptEntry => ({
      kind: "message",
      message,
    })),
    ...activities.map((item): TranscriptEntry => ({ kind: "activity", item })),
  ];
  entries.sort((left, right) => compareSequence(entrySequence(left), entrySequence(right)));

  const groups = new Map<string, { scope: DiagnosticScope; entries: TranscriptEntry[] }>();
  let observedElapsedNs: U64String = "0" as U64String;
  for (const entry of entries) {
    const elapsed = entryElapsed(entry);
    if (compareSequence(elapsed, observedElapsedNs) > 0) {
      observedElapsedNs = elapsed;
    }
    const scope = groupScope(entryScope(entry));
    const key = groupKey(scope);
    const group = groups.get(key);
    if (group === undefined) {
      groups.set(key, { scope, entries: [entry] });
    } else {
      group.entries.push(entry);
    }
  }

  const orderedGroups = [...groups.entries()].map(([key, group]): TranscriptGroup => ({
    key,
    scope: group.scope,
    firstSequence: entrySequence(group.entries[0]!),
    entries: group.entries,
  })).sort((left, right) => compareSequence(left.firstSequence, right.firstSequence));

  return {
    groups: orderedGroups,
    observedElapsedNs,
    messagesIncomplete: edge.projection.messages.needs_server_refresh,
    activityIncomplete: edge.projection.spans.needs_server_refresh
      || edge.projection.tools.needs_server_refresh
      || edge.projection.results.needs_server_refresh
      || edge.dropped_through !== null,
  };
}

function ScopeValue({ label, value }: { readonly label: string; readonly value: string | null }): JSX.Element {
  return (
    <div>
      <dt>{label}</dt>
      <dd>{value ?? "Unknown"}</dd>
    </div>
  );
}

function entryKey(entry: TranscriptEntry): string {
  if (entry.kind === "message") {
    return `message:${entry.message.message_id}`;
  }
  return toolResultKey(entry.item);
}

export function TranscriptPanel({
  state,
  onSelectionChange,
}: TranscriptPanelProps): JSX.Element {
  const model = selectTranscriptModel(state);
  const incomplete = model.messagesIncomplete || model.activityIncomplete;
  return (
    <section class="diagnostic-transcript" aria-label="Agent transcript">
      <header class="diagnostic-transcript__header">
        <div>
          <h2>Agent transcript</h2>
          <span>{model.groups.length} active scope{model.groups.length === 1 ? "" : "s"}</span>
        </div>
        {incomplete ? (
          <div class="diagnostic-transcript__gap" role="status">
            <AlertTriangle aria-hidden="true" size={17} strokeWidth={1.75} />
            <span>Some transcript history is outside the bounded live window.</span>
          </div>
        ) : null}
      </header>

      <div class="diagnostic-transcript__scroll" data-testid="transcript-scroll">
        {model.groups.length === 0 ? (
          <p class="diagnostic-transcript__empty">No agent transcript is available.</p>
        ) : model.groups.map((group) => (
          <section
            key={group.key}
            class="transcript-scope"
            data-actor-id={group.scope.actor_id ?? ""}
            data-cue-id={group.scope.cue_id ?? ""}
            data-act-id={group.scope.act_id ?? ""}
            aria-label={[
              `Actor ${group.scope.actor_id ?? "unknown"}`,
              `cue ${group.scope.cue_id ?? "unknown"}`,
              `act ${group.scope.act_id ?? "unknown"}`,
            ].join(", ")}
          >
            <header class="transcript-scope__header">
              <h3>Actor {group.scope.actor_id ?? "Unknown"}</h3>
              <dl>
                <ScopeValue label="Scene" value={group.scope.scene_id} />
                <ScopeValue label="Cue" value={group.scope.cue_id} />
                <ScopeValue label="Act" value={group.scope.act_id} />
              </dl>
            </header>
            <ol class="transcript-scope__entries">
              {group.entries.map((entry) => (
                <li key={entryKey(entry)}>
                  {entry.kind === "message" ? (
                    <MessageStreamItem
                      message={entry.message}
                      selection={state.presentation.selection}
                      onSelectionChange={onSelectionChange}
                    />
                  ) : (
                    <ToolResultRow
                      item={entry.item}
                      observedElapsedNs={model.observedElapsedNs}
                      selection={state.presentation.selection}
                      onSelectionChange={onSelectionChange}
                    />
                  )}
                </li>
              ))}
            </ol>
          </section>
        ))}
      </div>
    </section>
  );
}
