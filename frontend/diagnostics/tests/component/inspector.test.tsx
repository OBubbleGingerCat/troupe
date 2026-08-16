import { readFileSync } from "node:fs";
import { resolve } from "node:path";

import {
  cleanup,
  fireEvent,
  render,
  screen,
  within,
} from "@testing-library/preact";
import "@testing-library/jest-dom/vitest";
import {
  afterEach,
  describe,
  expect,
  it,
  vi,
} from "vitest";

import { decodeU64 } from "../../src/protocol/decimal.ts";
import {
  DIAGNOSTIC_EVENT_KINDS,
  type DiagnosticEvent,
  type DiagnosticScope,
  decodeDiagnosticEvent,
} from "../../src/protocol/event.ts";
import { EventInspector } from "../../src/inspector/EventInspector.tsx";
import {
  EventTable,
  type EventPageRequest,
} from "../../src/inspector/EventTable.tsx";
import {
  EMPTY_EVENT_QUERY,
  FilterBar,
  type EventQueryState,
} from "../../src/inspector/FilterBar.tsx";
import {
  eventSelectionHighlight,
  resolveSelection,
  selectionHighlightsScope,
  selectionOverlapsElapsedRange,
} from "../../src/inspector/selection.ts";
import {
  eventReference,
  messageReference,
  scopeReference,
  spanReference,
} from "../../src/state/selection.ts";
import {
  loadAllValidEventFixtures,
  loadEventFixture,
} from "../support/diagnostic-fixtures.ts";


const RUN_ID = "12345678-1234-4234-9234-123456789abc";
const INSPECTOR_CSS = readFileSync(
  resolve(process.cwd(), "src/inspector/inspector.css"),
  "utf8",
);
const ACTOR_SCOPE = {
  scene_id: "scene-1",
  actor_id: "actor-1",
  cue_id: "cue-1",
  effect_id: null,
  act_id: "act-1",
  tool_call_id: null,
  session_generation: "1",
} as const;

afterEach(() => cleanup());

function decode(raw: unknown): DiagnosticEvent {
  return decodeDiagnosticEvent(raw);
}

function messageDelta(
  sequence: string,
  elapsedNs: string,
  text: string,
  messageId = "message-1",
): DiagnosticEvent {
  return decode({
    kind: "agent_message_delta",
    schema_version: 1,
    run_id: RUN_ID,
    sequence,
    elapsed_ns: elapsedNs,
    scope: ACTOR_SCOPE,
    caused_by: [],
    message_id: messageId,
    source_message_id: null,
    text_delta: text,
  });
}

function toolUpdate(sequence: string, elapsedNs: string): DiagnosticEvent {
  return decode({
    kind: "instant_occurred",
    schema_version: 1,
    run_id: RUN_ID,
    sequence,
    elapsed_ns: elapsedNs,
    scope: { ...ACTOR_SCOPE, tool_call_id: "tool-1" },
    caused_by: [{ source_sequence: "1", relation: "follows_from" }],
    instant_kind: "tool.updated",
    detail: {
      title: "Read a very long path",
      tool_kind: "read",
      status: "in_progress",
      error_code: null,
    },
    containing_span_id: null,
  });
}

describe("diagnostic event table, inspector, and query linkage", () => {
  it("renders the supplied page without filtering facts and emits typed page and selection intents", () => {
    const events = [
      messageDelta("1", "10", "streaming text"),
      toolUpdate("2", "15"),
      decode({
        kind: "observation_gap",
        schema_version: 1,
        run_id: RUN_ID,
        sequence: "3",
        elapsed_ns: "20",
        scope: ACTOR_SCOPE,
        caused_by: [],
        producer: "runtime",
        component: null,
        reason: "unknown_source_loss",
        dropped_count: null,
        affected_elapsed: null,
        affected_kind: null,
        affected_scope: null,
      }),
    ];
    const onSelectionChange = vi.fn();
    const onPageRequest = vi.fn<(request: EventPageRequest) => void>();

    const { container } = render(
      <EventTable
        page={{
          events,
          captured_through: decodeU64("12"),
          previous: { after: null },
          next: { after: decodeU64("3") },
        }}
        selection={messageReference("message-1")}
        onSelectionChange={onSelectionChange}
        onPageRequest={onPageRequest}
      />,
    );

    expect(screen.getAllByRole("row")).toHaveLength(4);
    expect(container.querySelector('[data-event-sequence="1"]')).toHaveAttribute("data-selection", "selected");
    expect(container.querySelector('[data-event-sequence="2"]')).toHaveAttribute("data-selection", "related");
    expect(screen.getByText("Observation gap: unknown_source_loss")).toBeInTheDocument();

    fireEvent.click(screen.getByRole("button", { name: "Select event 3" }));
    expect(onSelectionChange).toHaveBeenLastCalledWith(eventReference(events[2]!.sequence));

    fireEvent.click(screen.getByRole("button", { name: "streaming text" }));
    expect(onSelectionChange).toHaveBeenLastCalledWith(messageReference("message-1"));

    fireEvent.click(screen.getByRole("button", { name: "tool.updated" }));
    expect(onSelectionChange).toHaveBeenLastCalledWith(scopeReference(events[1]!.scope));

    fireEvent.click(screen.getByRole("button", { name: "Previous event page" }));
    fireEvent.click(screen.getByRole("button", { name: "Next event page" }));
    expect(onPageRequest.mock.calls).toEqual([
      [{ direction: "previous", cursor: { after: null } }],
      [{ direction: "next", cursor: { after: "3" } }],
    ]);
  });

  it("emits actor, event-kind, and error query state without applying a local event filter", () => {
    const onQueryChange = vi.fn<(query: EventQueryState) => void>();
    const { rerender } = render(
      <FilterBar
        query={EMPTY_EVENT_QUERY}
        actors={[
          { id: "actor-1", label: "Research actor" },
          { id: "actor-2", label: "<img src=x onerror=alert(1)>" },
        ]}
        onQueryChange={onQueryChange}
      />,
    );

    fireEvent.change(screen.getByLabelText("Actor filter"), { target: { value: "actor-2" } });
    expect(onQueryChange).toHaveBeenLastCalledWith({
      ...EMPTY_EVENT_QUERY,
      actor_id: "actor-2",
    });

    const kinds = screen.getByLabelText("Event type filters") as HTMLSelectElement;
    const gapOption = within(kinds).getByRole("option", { name: "Observation Gap" }) as HTMLOptionElement;
    gapOption.selected = true;
    fireEvent.change(kinds);
    expect(onQueryChange).toHaveBeenLastCalledWith({
      ...EMPTY_EVENT_QUERY,
      event_kinds: ["observation_gap"],
    });

    fireEvent.change(screen.getByLabelText("Error filter"), {
      target: { value: "errors_and_gaps" },
    });
    expect(onQueryChange).toHaveBeenLastCalledWith({
      ...EMPTY_EVENT_QUERY,
      error_filter: "errors_and_gaps",
    });
    expect(document.querySelector("img")).toBeNull();

    rerender(
      <FilterBar
        query={{
          actor_id: "actor-2",
          event_kinds: ["observation_gap"],
          error_filter: "errors_and_gaps",
          scene_id: null,
          text: "",
        }}
        actors={[]}
        onQueryChange={onQueryChange}
      />,
    );
    fireEvent.click(screen.getByRole("button", { name: "Clear event filters" }));
    expect(onQueryChange).toHaveBeenLastCalledWith(EMPTY_EVENT_QUERY);
  });

  it("has an explicit typed detail branch for every diagnostic event variant", () => {
    const byKind = new Map<string, DiagnosticEvent>();
    for (const raw of loadAllValidEventFixtures()) {
      const event = decode(raw);
      if (!byKind.has(event.kind)) {
        byKind.set(event.kind, event);
      }
    }
    expect([...byKind.keys()].sort()).toEqual([...DIAGNOSTIC_EVENT_KINDS].sort());

    const view = render(<EventInspector event={null} />);
    for (const kind of DIAGNOSTIC_EVENT_KINDS) {
      view.rerender(<EventInspector event={byKind.get(kind)!} />);
      expect(screen.getByRole("heading", { name: kind })).toBeInTheDocument();
      expect(screen.getByRole("heading", { name: "Typed detail" })).toBeInTheDocument();
    }
  });

  it("emits W02-compatible canonical scope and span references from the inspector", () => {
    const tool = toolUpdate("2", "15");
    const onSelectionChange = vi.fn();
    const view = render(
      <EventInspector event={tool} onSelectionChange={onSelectionChange} />,
    );
    const actorScope: DiagnosticScope = {
      scene_id: tool.scope.scene_id,
      actor_id: tool.scope.actor_id,
      cue_id: null,
      effect_id: null,
      act_id: null,
      tool_call_id: null,
      session_generation: tool.scope.session_generation,
    };

    fireEvent.click(screen.getByRole("button", { name: "actor-1" }));
    expect(onSelectionChange).toHaveBeenLastCalledWith(scopeReference(actorScope));

    const spanEvents = (loadEventFixture("span-finished.json") as readonly unknown[]).map(decode);
    const finish = spanEvents.find((event) => event.kind === "span_finished")!;
    if (finish.kind !== "span_finished") {
      throw new Error("span fixture did not decode as span_finished");
    }
    view.rerender(
      <EventInspector event={finish} onSelectionChange={onSelectionChange} />,
    );
    fireEvent.click(screen.getByRole("button", { name: finish.span_id }));
    expect(onSelectionChange).toHaveBeenLastCalledWith(spanReference(finish.span_id));
  });

  it("keeps gaps, truncation, unknown optionals, and arbitrary token integers readable", () => {
    const limits = (loadEventFixture("limits.json") as readonly unknown[]).map(decode);
    const usage = limits.find((event) => event.kind === "act_token_usage_finalized")!;
    const gaps = (loadEventFixture("observation-gap.json") as readonly unknown[]).map(decode);
    const unknownGap = gaps[1]!;
    const completions = (loadEventFixture("agent-message-completed.json") as readonly unknown[]).map(decode);
    const truncated = completions[1]!;
    const view = render(<EventInspector event={usage} />);

    expect(screen.getByText(
      "12345678901234567890123456789012345678901234567890123456789012345678901234567890",
    )).toBeInTheDocument();
    expect(screen.getAllByText("Unknown").length).toBeGreaterThan(0);

    view.rerender(<EventInspector event={unknownGap} />);
    expect(screen.getByText("Some observations are unavailable for this interval.")).toBeInTheDocument();
    expect(screen.getAllByText("Unknown").length).toBeGreaterThanOrEqual(5);

    view.rerender(<EventInspector event={truncated} />);
    expect(screen.getByText("Yes, content is incomplete")).toBeInTheDocument();
  });

  it("renders payload-like user strings only as text and enforces the long-word overflow contract", () => {
    const payload = `<script>globalThis.compromised=true</script>${"x".repeat(400)}`;
    const event = messageDelta("1", "10", payload);
    const inspector = render(<EventInspector event={event} />);
    const message = screen.getByText(payload);

    expect(message).toHaveTextContent(payload);
    expect(inspector.container.querySelector("script, img, a")).toBeNull();
    expect(message).toHaveClass("diagnostic-long-content");
    expect(INSPECTOR_CSS).toMatch(
      /\.diagnostic-long-content\s*\{[^}]*overflow-wrap: anywhere;[^}]*word-break: break-word;/s,
    );

    cleanup();
    render(
      <EventTable
        page={{
          events: [event],
          captured_through: decodeU64("1"),
          previous: null,
          next: null,
        }}
        selection={null}
        onSelectionChange={() => undefined}
        onPageRequest={() => undefined}
      />,
    );
    expect(screen.getByTestId("event-table-scroll")).toHaveClass("diagnostic-event-table__scroll");
    expect(screen.getByTestId("event-summary-1")).toHaveClass("diagnostic-long-content");
    expect(INSPECTOR_CSS).toMatch(
      /\.diagnostic-event-table__scroll\s*\{[^}]*overflow-x: auto;/s,
    );
  });

  it("resolves one shared message selection into table, tree-scope, and timeline-range highlights", () => {
    const events = [
      messageDelta("1", "10", "first"),
      toolUpdate("2", "15"),
      messageDelta("3", "20", "second"),
    ];
    const selection = messageReference("message-1");
    const resolved = resolveSelection(selection, events);
    const actorScope: DiagnosticScope = {
      scene_id: null,
      actor_id: "actor-1",
      cue_id: null,
      effect_id: null,
      act_id: null,
      tool_call_id: null,
      session_generation: null,
    };

    expect(resolved?.elapsed_range).toEqual({ start_ns: "10", end_ns: "20" });
    expect(eventSelectionHighlight(events[1]!, selection, events)).toBe("related");
    expect(selectionHighlightsScope(actorScope, selection, events)).toBe(true);
    expect(selectionOverlapsElapsedRange({
      start_ns: decodeU64("12"),
      end_ns: decodeU64("18"),
    }, selection, events)).toBe(true);
  });
});
