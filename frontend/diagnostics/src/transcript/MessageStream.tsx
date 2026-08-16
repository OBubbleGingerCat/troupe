import { MessageSquareText } from "lucide-preact";
import type { JSX } from "preact";

import type {
  ProjectedMessage,
  SelectionReference,
} from "../state/model.ts";
import {
  messageReference,
  sameSelectionReference,
} from "../state/selection.ts";


export interface MessageStreamProps {
  readonly messages: readonly ProjectedMessage[];
  readonly selection: SelectionReference | null;
  readonly onSelectionChange?: ((selection: SelectionReference) => void) | undefined;
}

export interface MessageStreamItemProps {
  readonly message: ProjectedMessage;
  readonly selection: SelectionReference | null;
  readonly onSelectionChange?: ((selection: SelectionReference) => void) | undefined;
}

function completionLabel(message: ProjectedMessage): string {
  if (message.completion === null) {
    return "Streaming";
  }
  return message.completion.truncated ? "Completed, truncated" : "Completed";
}

function incompletenessNotes(message: ProjectedMessage): readonly string[] {
  const notes: string[] = [];
  if (!message.text_complete_from_start) {
    notes.push("The beginning of this message is outside the captured range.");
  }
  if (message.text_truncated_before) {
    notes.push("Earlier message text was removed by the bounded transcript window.");
  }
  if (message.completion?.truncated === true) {
    notes.push("The provider reported truncated message output.");
  }
  return notes;
}

export function MessageStreamItem({
  message,
  selection,
  onSelectionChange,
}: MessageStreamItemProps): JSX.Element {
  const reference = messageReference(message.message_id);
  const selected = sameSelectionReference(selection, reference);
  const notes = incompletenessNotes(message);
  return (
    <article
      class="transcript-message"
      data-message-id={message.message_id}
      data-selected={selected}
      data-state={message.completion === null ? "streaming" : "completed"}
    >
      <header class="transcript-row-header">
        <button
          type="button"
          class="transcript-select-button"
          aria-label={`Select message ${message.message_id}`}
          title={`Select message ${message.message_id}`}
          disabled={onSelectionChange === undefined}
          onClick={() => onSelectionChange?.(reference)}
        >
          <MessageSquareText aria-hidden="true" size={17} strokeWidth={1.75} />
        </button>
        <div class="transcript-row-heading">
          <h4>Agent message</h4>
          <span>{message.message_id}</span>
        </div>
        <span class="transcript-status" data-status={message.completion === null ? "running" : "complete"}>
          {completionLabel(message)}
        </span>
      </header>

      {notes.length === 0 ? null : (
        <ul class="transcript-notices" aria-label="Message completeness">
          {notes.map((note) => <li key={note}>{note}</li>)}
        </ul>
      )}

      <pre
        class="transcript-message__text"
        data-testid={`message-text-${message.message_id}`}
      >{message.text}</pre>

      <dl class="transcript-metadata">
        <div>
          <dt>Sequence</dt>
          <dd>{message.first_sequence} to {message.latest_sequence}</dd>
        </div>
        {message.source_message_id === null ? null : (
          <div>
            <dt>Source</dt>
            <dd>{message.source_message_id}</dd>
          </div>
        )}
        {message.completion === null ? null : (
          <>
            <div>
              <dt>UTF-8 bytes</dt>
              <dd>{message.completion.utf8_bytes}</dd>
            </div>
            <div>
              <dt>Unicode scalars</dt>
              <dd>{message.completion.unicode_scalar_count}</dd>
            </div>
          </>
        )}
      </dl>
    </article>
  );
}

export function MessageStream({
  messages,
  selection,
  onSelectionChange,
}: MessageStreamProps): JSX.Element {
  return (
    <div class="transcript-message-stream">
      {messages.map((message) => (
        <MessageStreamItem
          key={message.message_id}
          message={message}
          selection={selection}
          onSelectionChange={onSelectionChange}
        />
      ))}
    </div>
  );
}
