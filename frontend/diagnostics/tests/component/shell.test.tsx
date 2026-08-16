import { readFileSync } from "node:fs";
import { resolve } from "node:path";

import { cleanup, fireEvent, render, screen, within } from "@testing-library/preact";
import { useReducer, useState } from "preact/hooks";
import { afterEach, describe, expect, it, vi } from "vitest";

import { AppShell } from "../../src/shell/AppShell.tsx";
import type { PrimarySection } from "../../src/shell/PrimaryToolbar.tsx";
import { decodeCanonicalUuid, decodeU64 } from "../../src/protocol/decimal.ts";
import {
  type DiagnosticEvent,
  type DiagnosticScope,
  type SpanStartedEvent,
  decodeDiagnosticEvent,
} from "../../src/protocol/event.ts";
import {
  createDiagnosticState,
  reduceDiagnosticState,
} from "../../src/state/reducer.ts";
import { scopeReference } from "../../src/state/selection.ts";


const RUN_ID = decodeCanonicalUuid("12345678-1234-4234-9234-123456789abc");
const NO_SCOPE: DiagnosticScope = {
  scene_id: null,
  actor_id: null,
  cue_id: null,
  effect_id: null,
  act_id: null,
  tool_call_id: null,
  session_generation: null,
};

afterEach(cleanup);

function scope(
  cueId: string | null = null,
  actId: string | null = null,
  toolCallId: string | null = null,
): DiagnosticScope {
  return {
    scene_id: "scene-0042",
    actor_id: "actor-investigator",
    cue_id: cueId,
    effect_id: null,
    act_id: actId,
    tool_call_id: toolCallId,
    session_generation: decodeU64("1"),
  };
}

function spanStarted(
  sequence: number,
  spanKind: SpanStartedEvent["span_kind"],
  eventScope: DiagnosticScope,
  detail: Readonly<Record<string, unknown>> = {},
): DiagnosticEvent {
  return decodeDiagnosticEvent({
    kind: "span_started",
    schema_version: 1,
    run_id: RUN_ID,
    sequence: String(sequence),
    elapsed_ns: String(sequence * 10),
    scope: eventScope,
    caused_by: [],
    span_kind: spanKind,
    detail,
    parent_span_id: null,
  });
}

function spanFinished(
  sequence: number,
  spanId: number,
  eventScope: DiagnosticScope,
  outcome: "completed" | "cancelled" | "failed" = "completed",
): DiagnosticEvent {
  return decodeDiagnosticEvent({
    kind: "span_finished",
    schema_version: 1,
    run_id: RUN_ID,
    sequence: String(sequence),
    elapsed_ns: String(sequence * 10),
    scope: eventScope,
    caused_by: [],
    span_id: String(spanId),
    outcome,
    error_code: outcome === "failed" ? "fixture_failure" : null,
  });
}

function multiCueState() {
  const cue102 = scope("c-102");
  const cue102Act = scope("c-102", "act-1");
  const cue102Tool = scope("c-102", "act-1", "tool-1");
  const cue103 = scope("c-103");
  const cue103Act = scope("c-103", "act-2");
  const cue104 = scope("c-104");
  const events = [
    spanStarted(1, "run.lifecycle", NO_SCOPE),
    spanStarted(2, "scene.lifecycle", { ...scope(), actor_id: null }),
    spanStarted(3, "actor.handle_lifetime", scope(), {
      display_name: "Investigator",
      actor_type: "ResearchActor",
    }),
    spanStarted(4, "cue.mailbox_wait", cue102),
    spanFinished(5, 4, cue102),
    spanStarted(6, "cue.execution", cue102),
    spanStarted(7, "act.lifecycle", cue102Act, {
      provider: "codex",
      effective_model: "gpt-5",
      effective_effort: "high",
    }),
    spanStarted(8, "tool.call", cue102Tool, {
      title: "Search records",
      tool_kind: "search",
      status: "completed",
      error_code: null,
    }),
    spanFinished(9, 8, cue102Tool),
    spanFinished(10, 7, cue102Act),
    spanFinished(11, 6, cue102),
    spanStarted(12, "cue.mailbox_wait", cue103),
    spanFinished(13, 12, cue103),
    spanStarted(14, "cue.execution", cue103),
    spanStarted(15, "act.lifecycle", cue103Act, {
      provider: "codex",
      effective_model: "gpt-5",
      effective_effort: "high",
    }),
    spanStarted(16, "cue.mailbox_wait", cue104),
  ];
  return events.reduce(
    (state, event) => reduceDiagnosticState(state, { type: "event_received", event }),
    createDiagnosticState(RUN_ID, decodeU64("0")),
  );
}

function ShellHarness() {
  const [state, dispatch] = useReducer(reduceDiagnosticState, multiCueState());
  const [section, setSection] = useState<PrimarySection>("timeline");
  return (
    <AppShell
      state={state}
      productionName="research-production"
      connection="connected"
      outcome="running"
      securityScope="trusted_network"
      activeSection={section}
      dispatch={dispatch}
      onSectionChange={setSection}
    >
      <section aria-label="Selected work area">Selected panel</section>
    </AppShell>
  );
}

describe("diagnostics workbench shell", () => {
  it("opens on the operational workbench with prominent run identity and navigation", () => {
    render(<ShellHarness />);

    expect(screen.getByText("Troupe Diagnostics")).toBeTruthy();
    expect(screen.getAllByText("research-production").length).toBeGreaterThan(0);
    expect(screen.getByText(RUN_ID)).toBeTruthy();
    expect(screen.getByText("Connected")).toBeTruthy();
    expect(screen.getByText("trusted_network")).toBeTruthy();
    expect(within(screen.getByLabelText("Run status")).getByText("running")).toBeTruthy();
    expect(screen.getByRole("tree", { name: "Production execution" })).toBeTruthy();
    expect(screen.getByRole("tabpanel").getAttribute("aria-labelledby")).toBe(
      "diagnostic-tab-timeline",
    );

    const tabs = screen.getAllByRole("tab");
    expect(tabs.map((tab) => tab.textContent)).toEqual([
      "Timeline",
      "Agent",
      "Events",
      "Usage",
      "Views",
    ]);
    expect(screen.getByRole("tab", { name: "Timeline" }).getAttribute("aria-selected")).toBe(
      "true",
    );
    expect(screen.getByRole("heading", { level: 1 }).textContent).toBe("Troupe Diagnostics");
  });

  it("keeps same-actor cues independent and retains cue stages while collapsed", () => {
    render(<ShellHarness />);

    const actorButton = screen.getByRole("button", {
      name: "Investigator, actor-investigator",
    });
    const actorRow = actorButton.closest(".execution-tree__row");
    expect(actorRow?.textContent).toContain("1 done / 1 running / 1 queued");
    expect(actorRow?.querySelector(".execution-tree__status")).toBeNull();

    const completedCue = screen.getByRole("button", { name: "Cue c-102" });
    const completedRow = completedCue.closest(".execution-tree__row");
    expect(completedRow?.textContent).toContain("wait completed");
    expect(completedRow?.textContent).toContain("execution completed");
    expect(completedRow?.textContent).toContain("completed");
    expect(screen.queryByRole("button", { name: "Act act-1, gpt-5" })).toBeNull();

    fireEvent.click(screen.getByRole("button", { name: "Expand Cue c-102" }));
    expect(screen.getByRole("button", { name: "Collapse Cue c-102" })).toBeTruthy();
    expect(screen.getByRole("button", { name: "Mailbox wait" })).toBeTruthy();
    expect(screen.getByRole("button", { name: "Actor.cued()" })).toBeTruthy();
    expect(screen.getByRole("button", { name: "Act act-1, gpt-5" })).toBeTruthy();
    expect(screen.getByRole("button", { name: "Search records, tool-1" })).toBeTruthy();
    expect(screen.queryByRole("button", { name: "Act act-2, gpt-5" })).toBeNull();

    fireEvent.click(screen.getByRole("button", { name: "Expand Cue c-103" }));
    expect(screen.getByRole("button", { name: "Act act-2, gpt-5" })).toBeTruthy();
    fireEvent.click(screen.getByRole("button", { name: "Collapse Cue c-102" }));
    expect(screen.queryByRole("button", { name: "Act act-1, gpt-5" })).toBeNull();
    expect(screen.getByRole("button", { name: "Act act-2, gpt-5" })).toBeTruthy();

    const queuedCue = screen.getByRole("button", { name: "Cue c-104" });
    expect(queuedCue.closest(".execution-tree__row")?.textContent).toContain("wait waiting");
    expect(queuedCue.closest(".execution-tree__row")?.textContent).toContain("execution queued");
  });

  it("dispatches shared selection and pause actions while tabs remain controlled", () => {
    render(<ShellHarness />);

    const cue = screen.getByRole("button", { name: "Cue c-103" });
    fireEvent.click(cue);
    expect(cue.closest("[role='treeitem']")?.getAttribute("aria-selected")).toBe("true");

    const timelineTab = screen.getByRole("tab", { name: "Timeline" });
    timelineTab.focus();
    fireEvent.keyDown(timelineTab, { key: "ArrowRight" });
    expect(screen.getByRole("tab", { name: "Agent" }).getAttribute("aria-selected")).toBe(
      "true",
    );
    expect(document.activeElement).toBe(screen.getByRole("tab", { name: "Agent" }));

    const pause = screen.getByRole("button", { name: "Pause live presentation" });
    expect(pause.getAttribute("title")).toBe("Pause live presentation");
    fireEvent.click(pause);
    expect(screen.getByRole("button", { name: "Resume live presentation" })).toBeTruthy();

    fireEvent.click(screen.getByRole("tab", { name: "Events" }));
    expect(screen.getByRole("tab", { name: "Events" }).getAttribute("aria-selected")).toBe(
      "true",
    );
    expect(screen.getByRole("tabpanel").getAttribute("aria-labelledby")).toBe(
      "diagnostic-tab-events",
    );
    expect(within(screen.getByRole("tabpanel")).getByText("Selected panel")).toBeTruthy();
  });

  it("shows exact paused backlog and preserves accessible icon tooltips", () => {
    let state = reduceDiagnosticState(multiCueState(), { type: "pause" });
    state = reduceDiagnosticState(state, {
      type: "watermark_observed",
      through_sequence: decodeU64("18"),
    });
    const dispatch = vi.fn();
    render(
      <AppShell
        state={state}
        productionName="research-production"
        connection="reconnecting"
        outcome="incomplete"
        securityScope="trusted_network"
        activeSection="timeline"
        dispatch={dispatch}
        onSectionChange={vi.fn()}
      />,
    );

    expect(screen.getByText("2 unseen")).toBeTruthy();
    const resume = screen.getByRole("button", { name: "Resume live presentation" });
    expect(resume.getAttribute("title")).toBe("Resume live presentation");
    const toggle = screen.getByRole("button", { name: "Expand Cue c-102" });
    expect(toggle.getAttribute("title")).toBe("Expand Cue c-102");
    fireEvent.click(screen.getByRole("button", { name: "Cue c-103" }));
    expect(dispatch).toHaveBeenCalledWith({
      type: "select",
      selection: scopeReference(scope("c-103")),
    });
    fireEvent.click(resume);
    expect(dispatch).toHaveBeenCalledWith({ type: "resume" });
  });

  it("locks responsive geometry and consumes W08 projection selectors", () => {
    const css = readFileSync(resolve(process.cwd(), "src/shell/shell.css"), "utf8");
    const selectors = readFileSync(resolve(process.cwd(), "src/shell/selectors.ts"), "utf8");

    expect(css).toContain(
      "grid-template-columns: minmax(17rem, 23rem) minmax(0, 1fr)",
    );
    expect(css).toContain("@media (max-width: 48rem)");
    expect(css).toContain("grid-template-columns: minmax(0, 1fr)");
    expect(css).toContain("grid-template-rows: minmax(12rem, 38dvh) minmax(18rem, 1fr)");
    expect(css).toContain("overflow-wrap: anywhere");
    expect(css).toContain("--troupe-control-size: 2.25rem");
    expect(css).toContain("min-width: var(--troupe-control-size)");
    expect(css).toContain("height: var(--troupe-control-size)");
    const radii = [...css.matchAll(/border-radius:\s*([0-9]+)px/g)]
      .map((match) => Number(match[1]));
    expect(radii.length).toBeGreaterThan(0);
    expect(radii.every((radius) => radius <= 8)).toBe(true);
    expect(css).not.toContain(".card");

    expect(selectors).toContain("presentedLiveEdge(state)");
    expect(selectors).toContain("edge.projection.spans.items");
    expect(selectors).not.toContain("state.live.events");
    expect(selectors).not.toContain("reduceDiagnosticState");
    expect(selectors).not.toContain("decodeDiagnosticEvent");
  });
});
