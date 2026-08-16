import { MessageSquareText } from "lucide-preact";
import { Component } from "preact";
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

interface TextSelectionSnapshot {
  readonly start: number;
  readonly end: number;
}

interface StableMessageTextProps {
  readonly messageId: string;
  readonly text: string;
}

function textOffset(root: Node, node: Node, offset: number): number {
  const range = document.createRange();
  range.selectNodeContents(root);
  range.setEnd(node, offset);
  return range.toString().length;
}

function textPoint(root: Node, offset: number): readonly [Node, number] {
  const walker = document.createTreeWalker(root, NodeFilter.SHOW_TEXT);
  let remaining = offset;
  let node = walker.nextNode();
  while (node !== null) {
    const length = node.textContent?.length ?? 0;
    if (remaining <= length) {
      return [node, remaining];
    }
    remaining -= length;
    node = walker.nextNode();
  }
  return [root, root.childNodes.length];
}

class StableMessageText extends Component<StableMessageTextProps, Record<never, never>> {
  private element: HTMLPreElement | null = null;

  private readonly setElement = (element: HTMLPreElement | null): void => {
    this.element = element;
  };

  override getSnapshotBeforeUpdate(
    previousProps: Readonly<StableMessageTextProps>,
  ): TextSelectionSnapshot | null {
    const root = this.element;
    const selection = window.getSelection();
    if (
      root === null
      || selection === null
      || selection.rangeCount === 0
      || !this.props.text.startsWith(previousProps.text)
    ) {
      return null;
    }
    const range = selection.getRangeAt(0);
    if (!root.contains(range.startContainer) || !root.contains(range.endContainer)) {
      return null;
    }
    return {
      start: textOffset(root, range.startContainer, range.startOffset),
      end: textOffset(root, range.endContainer, range.endOffset),
    };
  }

  override componentDidUpdate(
    _previousProps: Readonly<StableMessageTextProps>,
    _previousState: Readonly<Record<never, never>>,
    snapshot: TextSelectionSnapshot | null,
  ): void {
    const root = this.element;
    const selection = window.getSelection();
    if (root === null || selection === null || snapshot === null) {
      return;
    }
    const start = textPoint(root, snapshot.start);
    const end = textPoint(root, snapshot.end);
    const range = document.createRange();
    range.setStart(start[0], start[1]);
    range.setEnd(end[0], end[1]);
    selection.removeAllRanges();
    selection.addRange(range);
  }

  render(): JSX.Element {
    return (
      <pre
        ref={this.setElement}
        class="transcript-message__text"
        data-testid={`message-text-${this.props.messageId}`}
      >{this.props.text}</pre>
    );
  }
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

      <StableMessageText messageId={message.message_id} text={message.text} />

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
