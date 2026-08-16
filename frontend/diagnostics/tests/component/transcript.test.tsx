import { cleanup, fireEvent, render, screen, within } from "@testing-library/preact";
import { afterEach, describe, expect, it, vi } from "vitest";

import type { JsonObject } from "../../src/protocol/decimal.ts";
import {
  decodeCanonicalUuid,
  decodeU64,
} from "../../src/protocol/decimal.ts";
import type {
  AgentMessageCompletedEvent,
  AgentMessageDeltaEvent,
  DiagnosticEvent,
  DiagnosticScope,
  InstantOccurredEvent,
  ObservationGapEvent,
  SpanFinishedEvent,
  SpanStartedEvent,
} from "../../src/protocol/event.ts";
import type { DiagnosticState } from "../../src/state/model.ts";
import {
  createDiagnosticState,
  reduceDiagnosticState,
} from "../../src/state/reducer.ts";
import {
  eventReference,
  hierarchyScopeReference,
  messageReference,
  spanReference,
} from "../../src/state/selection.ts";
import { TranscriptPanel } from "../../src/transcript/TranscriptPanel.tsx";


const RUN_ID = decodeCanonicalUuid("12345678-1234-4234-9234-123456789abc");

afterEach(cleanup);

function scope(
  actorId: string,
  cueId: string,
  actId: string,
  toolCallId: string | null = null,
): DiagnosticScope {
  return {
    scene_id: "scene-main",
    actor_id: actorId,
    cue_id: cueId,
    effect_id: null,
    act_id: actId,
    tool_call_id: toolCallId,
    session_generation: decodeU64("1"),
  };
}

function messageDelta(
  sequence: number,
  eventScope: DiagnosticScope,
  messageId: string,
  textDelta: string,
): AgentMessageDeltaEvent {
  return {
    kind: "agent_message_delta",
    schema_version: 1,
    run_id: RUN_ID,
    sequence: decodeU64(String(sequence)),
    elapsed_ns: decodeU64(String(sequence * 10)),
    scope: eventScope,
    caused_by: [],
    message_id: messageId,
    source_message_id: `source-${messageId}`,
    text_delta: textDelta,
  };
}

function messageCompleted(
  sequence: number,
  eventScope: DiagnosticScope,
  messageId: string,
  text: string,
  truncated = false,
): AgentMessageCompletedEvent {
  return {
    kind: "agent_message_completed",
    schema_version: 1,
    run_id: RUN_ID,
    sequence: decodeU64(String(sequence)),
    elapsed_ns: decodeU64(String(sequence * 10)),
    scope: eventScope,
    caused_by: [],
    message_id: messageId,
    utf8_bytes: decodeU64(String(new TextEncoder().encode(text).length)),
    unicode_scalar_count: decodeU64(String([...text].length)),
    truncated,
  };
}

function spanStarted(
  sequence: number,
  eventScope: DiagnosticScope,
  spanKind: SpanStartedEvent["span_kind"],
  detail: JsonObject,
): SpanStartedEvent {
  return {
    kind: "span_started",
    schema_version: 1,
    run_id: RUN_ID,
    sequence: decodeU64(String(sequence)),
    elapsed_ns: decodeU64(String(sequence * 10)),
    scope: eventScope,
    caused_by: [],
    span_kind: spanKind,
    detail,
    parent_span_id: null,
  };
}

function spanFinished(
  sequence: number,
  eventScope: DiagnosticScope,
  spanId: number,
  outcome: SpanFinishedEvent["outcome"] = "completed",
  errorCode: string | null = null,
): SpanFinishedEvent {
  return {
    kind: "span_finished",
    schema_version: 1,
    run_id: RUN_ID,
    sequence: decodeU64(String(sequence)),
    elapsed_ns: decodeU64(String(sequence * 10)),
    scope: eventScope,
    caused_by: [],
    span_id: decodeU64(String(spanId)),
    outcome,
    error_code: errorCode,
  };
}

function instant(
  sequence: number,
  eventScope: DiagnosticScope,
  instantKind: InstantOccurredEvent["instant_kind"],
  detail: JsonObject,
): InstantOccurredEvent {
  return {
    kind: "instant_occurred",
    schema_version: 1,
    run_id: RUN_ID,
    sequence: decodeU64(String(sequence)),
    elapsed_ns: decodeU64(String(sequence * 10)),
    scope: eventScope,
    caused_by: [],
    instant_kind: instantKind,
    detail,
    containing_span_id: null,
  };
}

function observationGap(
  sequence: number,
  eventScope: DiagnosticScope,
): ObservationGapEvent {
  return {
    kind: "observation_gap",
    schema_version: 1,
    run_id: RUN_ID,
    sequence: decodeU64(String(sequence)),
    elapsed_ns: decodeU64(String(sequence * 10)),
    scope: eventScope,
    caused_by: [],
    producer: "agent-runtime",
    component: "message-stream",
    reason: "bounded_delivery_gap",
    dropped_count: decodeU64("1"),
    affected_elapsed: {
      start_ns: decodeU64("1"),
      end_ns: decodeU64(String(sequence * 10)),
    },
    affected_kind: "agent_message_delta",
    affected_scope: eventScope,
  };
}

function receive(state: DiagnosticState, event: DiagnosticEvent): DiagnosticState {
  return reduceDiagnosticState(state, { type: "event_received", event });
}

function stateFrom(events: readonly DiagnosticEvent[], baseThrough = "0"): DiagnosticState {
  return events.reduce(
    receive,
    createDiagnosticState(RUN_ID, decodeU64(baseThrough)),
  );
}

function markMessageWindowTruncated(state: DiagnosticState): DiagnosticState {
  return {
    ...state,
    live: {
      ...state.live,
      projection: {
        ...state.live.projection,
        messages: {
          ...state.live.projection.messages,
          items: state.live.projection.messages.items.map((message) => ({
            ...message,
            text_truncated_before: true,
          })),
        },
      },
    },
  };
}

describe("agent transcript", () => {
  it("renders streaming plain text and preserves diagnostic selection and scroll across deltas", () => {
    const actScope = scope("actor-a", "cue-1", "act-1");
    const firstText = "hello <strong>literal</strong>\nline";
    const completeText = `${firstText} plus`;
    const first = reduceDiagnosticState(
      stateFrom([messageDelta(1, actScope, "message-1", firstText)]),
      { type: "select", selection: messageReference("message-1") },
    );
    const selected = vi.fn();
    const view = render(<TranscriptPanel state={first} onSelectionChange={selected} />);

    const scroll = screen.getByTestId("transcript-scroll");
    const text = screen.getByTestId("message-text-message-1");
    const row = text.closest(".transcript-message");
    expect(text.textContent).toBe(firstText);
    expect(text.querySelector("strong")).toBeNull();
    expect(row?.getAttribute("data-selected")).toBe("true");
    expect(within(row as HTMLElement).getByText("Streaming")).toBeTruthy();

    scroll.scrollTop = 91;
    const second = receive(first, messageDelta(2, actScope, "message-1", " plus"));
    view.rerender(<TranscriptPanel state={second} onSelectionChange={selected} />);

    const updatedText = screen.getByTestId("message-text-message-1");
    expect(updatedText).toBe(text);
    expect(updatedText.closest(".transcript-message")).toBe(row);
    expect(updatedText.textContent).toBe(completeText);
    expect(updatedText.closest(".transcript-message")?.getAttribute("data-selected")).toBe("true");
    expect(scroll.scrollTop).toBe(91);

    let completed = receive(
      second,
      messageCompleted(3, actScope, "message-1", completeText, true),
    );
    completed = receive(completed, observationGap(4, actScope));
    completed = markMessageWindowTruncated(completed);
    view.rerender(<TranscriptPanel state={completed} onSelectionChange={selected} />);

    expect(screen.getByText("Completed, truncated")).toBeTruthy();
    expect(screen.getByText("The provider reported truncated message output.")).toBeTruthy();
    expect(screen.getByText("Earlier message text was removed by the bounded transcript window.")).toBeTruthy();
    expect(screen.getByText("Some transcript history is outside the bounded live window.")).toBeTruthy();
    expect(
      within(row as HTMLElement).getByText("UTF-8 bytes").nextElementSibling?.textContent,
    ).toBe(String(new TextEncoder().encode(completeText).length));
    expect(
      within(row as HTMLElement).getByText("Unicode scalars").nextElementSibling?.textContent,
    ).toBe(String([...completeText].length));

    fireEvent.click(screen.getByRole("button", { name: "Select message message-1" }));
    expect(selected).toHaveBeenCalledWith(messageReference("message-1"));
  });

  it("shows tool lifecycle and result metadata while thinking exposes only state and duration", () => {
    const actScope = scope("actor-a", "cue-1", "act-1");
    const toolScope = scope("actor-a", "cue-1", "act-1", "tool-7");
    const state = stateFrom([
      spanStarted(1, actScope, "agent.thinking", { hidden: "private chain of thought" }),
      spanStarted(2, toolScope, "tool.call", {
        title: "Read <img src=x onerror=alert(1)>",
        tool_kind: "read",
        status: "in_progress",
        error_code: null,
      }),
      instant(3, toolScope, "tool.updated", {
        title: "Read workspace files",
        tool_kind: "read",
        status: "completed",
        error_code: null,
      }),
      instant(4, actScope, "result.submitted", { issue: null, error_code: null }),
      instant(5, actScope, "result.rejected", {
        issue: { code: "out_of_range", path: "/score" },
        error_code: "invalid_result",
      }),
      spanFinished(6, actScope, 1),
      spanFinished(7, toolScope, 2, "failed", "tool_failed"),
    ]);
    const projectedOnlyState: DiagnosticState = {
      ...state,
      live: { ...state.live, events: [] },
    };
    const selected = vi.fn();
    const { container } = render(
      <TranscriptPanel state={projectedOnlyState} onSelectionChange={selected} />,
    );

    const thinking = container.querySelector('[data-activity-kind="thinking"]') as HTMLElement;
    expect(thinking.textContent).toContain("Thinking");
    expect(thinking.textContent).toContain("completed");
    expect(thinking.textContent).toContain("50 ns");
    expect(thinking.textContent).not.toContain("private chain of thought");

    const started = container.querySelector(
      '[data-activity-kind="tool"][data-tool-phase="started"]',
    ) as HTMLElement;
    expect(started.textContent).toContain("Read <img src=x onerror=alert(1)>");
    expect(started.textContent).toContain("in_progress");
    expect(started.querySelector("img")).toBeNull();

    const update = container.querySelector(
      '[data-activity-kind="tool"][data-tool-phase="updated"]',
    ) as HTMLElement;
    expect(update.textContent).toContain("Read workspace files");
    expect(update.textContent).toContain("completed");

    const finished = container.querySelector(
      '[data-activity-kind="tool"][data-tool-phase="finished"]',
    ) as HTMLElement;
    expect(finished.textContent).toContain("failed");
    expect(finished.textContent).toContain("tool_failed");
    expect(finished.textContent).toContain("50 ns");

    const results = container.querySelectorAll('[data-activity-kind="result"]');
    expect(results).toHaveLength(2);
    expect(results[0]!.textContent).toContain("Result submitted");
    expect(results[0]!.textContent).toContain("MetadataNone");
    expect(results[1]!.textContent).toContain("Result rejected");
    expect(results[1]!.textContent).toContain("out_of_range");
    expect(results[1]!.textContent).toContain("/score");
    expect(results[1]!.textContent).toContain("invalid_result");

    fireEvent.click(screen.getByRole("button", { name: "Select thinking span 1" }));
    fireEvent.click(screen.getByRole("button", { name: "Select tool finished event 7" }));
    fireEvent.click(screen.getByRole("button", { name: "Select result.rejected event 5" }));
    expect(selected).toHaveBeenNthCalledWith(1, spanReference(decodeU64("1")));
    expect(selected).toHaveBeenNthCalledWith(
      2,
      hierarchyScopeReference(toolScope, "tool_call_id"),
    );
    expect(selected).toHaveBeenNthCalledWith(3, eventReference(decodeU64("5")));
  });

  it("keeps multiple actors, cues, and acts in exact independent transcript scopes", () => {
    const actorACue1 = scope("actor-a", "cue-1", "act-1");
    const actorACue2 = scope("actor-a", "cue-2", "act-2");
    const actorBCue1 = scope("actor-b", "cue-1", "act-9");
    let state = stateFrom([
      messageDelta(1, actorACue1, "message-a1", "actor A, first cue"),
      messageCompleted(2, actorACue1, "message-a1", "actor A, first cue"),
      messageDelta(3, actorACue2, "message-a2", "actor A, second cue"),
      messageDelta(4, actorBCue1, "message-b1", "actor B, independent cue"),
    ]);
    state = reduceDiagnosticState(state, {
      type: "select",
      selection: messageReference("message-a2"),
    });
    const selected = vi.fn();
    const { container } = render(
      <TranscriptPanel state={state} onSelectionChange={selected} />,
    );

    const groups = container.querySelectorAll(".transcript-scope");
    expect(groups).toHaveLength(3);
    const first = container.querySelector(
      '[data-actor-id="actor-a"][data-cue-id="cue-1"][data-act-id="act-1"]',
    ) as HTMLElement;
    const second = container.querySelector(
      '[data-actor-id="actor-a"][data-cue-id="cue-2"][data-act-id="act-2"]',
    ) as HTMLElement;
    const third = container.querySelector(
      '[data-actor-id="actor-b"][data-cue-id="cue-1"][data-act-id="act-9"]',
    ) as HTMLElement;
    expect(first.textContent).toContain("actor A, first cue");
    expect(first.textContent).not.toContain("second cue");
    expect(second.textContent).toContain("actor A, second cue");
    expect(second.textContent).not.toContain("first cue");
    expect(third.textContent).toContain("actor B, independent cue");
    expect(third.textContent).not.toContain("actor A");
    expect(
      second.querySelector('[data-message-id="message-a2"]')?.getAttribute("data-selected"),
    ).toBe("true");

    fireEvent.click(within(third).getByRole("button", { name: "Select message message-b1" }));
    expect(selected).toHaveBeenCalledWith(messageReference("message-b1"));
  });

  it("uses the paused W08 presentation edge without rebuilding transcript history", () => {
    const actScope = scope("actor-a", "cue-1", "act-1");
    let state = stateFrom([messageDelta(1, actScope, "message-1", "before pause")]);
    state = reduceDiagnosticState(state, { type: "pause" });
    state = receive(state, messageDelta(2, actScope, "message-1", " after pause"));
    const view = render(<TranscriptPanel state={state} />);

    expect(screen.getByTestId("message-text-message-1").textContent).toBe("before pause");
    state = reduceDiagnosticState(state, { type: "resume" });
    view.rerender(<TranscriptPanel state={state} />);
    expect(screen.getByTestId("message-text-message-1").textContent).toBe(
      "before pause after pause",
    );
  });

  it("advances open thinking duration from the W08 live clock", () => {
    const actScope = scope("actor-a", "cue-1", "act-1");
    const state = stateFrom([
      spanStarted(1, actScope, "agent.thinking", {}),
      {
        ...instant(2, actScope, "actor.cast", {}),
        elapsed_ns: decodeU64("90"),
      },
    ]);
    const projectedOnlyState: DiagnosticState = {
      ...state,
      live: { ...state.live, events: [] },
    };

    const { container } = render(<TranscriptPanel state={projectedOnlyState} />);
    const thinking = container.querySelector('[data-activity-kind="thinking"]') as HTMLElement;
    expect(thinking.textContent).toContain("80 ns");
  });
});
