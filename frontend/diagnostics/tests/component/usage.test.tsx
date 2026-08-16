import { readFileSync } from "node:fs";
import { resolve } from "node:path";

import {
  cleanup,
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
} from "vitest";

import {
  decodeCanonicalUuid,
  decodeTokenInteger,
  decodeU64,
} from "../../src/protocol/decimal.ts";
import {
  type DiagnosticEvent,
  type DiagnosticScope,
  decodeDiagnosticEvent,
} from "../../src/protocol/event.ts";
import {
  type UsageFieldAggregateSnapshot,
  decodeSnapshotResponse,
} from "../../src/protocol/http.ts";
import type { SelectedUsageAggregate } from "../../src/state/model.ts";
import {
  createDiagnosticState,
  reduceDiagnosticState,
} from "../../src/state/reducer.ts";
import { hierarchyScopeReference } from "../../src/state/selection.ts";
import { UsageCoverage } from "../../src/usage/UsageCoverage.tsx";
import { UsagePanel } from "../../src/usage/UsagePanel.tsx";
import {
  contextOccupancyPercent,
  formatExactInteger,
} from "../../src/usage/format.ts";
import { loadHttpFixture } from "../support/diagnostic-fixtures.ts";


const RUN_ID = decodeCanonicalUuid("12345678-1234-4234-9234-123456789abc");

afterEach(cleanup);

function actScope(actId: string): DiagnosticScope {
  return {
    scene_id: "scene-1",
    actor_id: "actor-1",
    cue_id: "cue-1",
    effect_id: null,
    act_id: actId,
    tool_call_id: null,
    session_generation: decodeU64("1"),
  };
}

function event(raw: unknown): DiagnosticEvent {
  return decodeDiagnosticEvent(raw);
}

function actStarted(sequence: number, actId: string): DiagnosticEvent {
  return event({
    kind: "span_started",
    schema_version: 1,
    run_id: RUN_ID,
    sequence: String(sequence),
    elapsed_ns: String(sequence * 10),
    scope: actScope(actId),
    caused_by: [],
    span_kind: "act.lifecycle",
    detail: {
      provider: "codex",
      effective_model: "gpt-5",
      effective_effort: "high",
    },
    parent_span_id: null,
  });
}

function contextSample(
  sequence: number,
  actId: string,
  used: string | null,
  size: string | null,
): DiagnosticEvent {
  return event({
    kind: "context_usage_sampled",
    schema_version: 1,
    run_id: RUN_ID,
    sequence: String(sequence),
    elapsed_ns: String(sequence * 10),
    scope: actScope(actId),
    caused_by: [],
    context_used_tokens: used,
    context_window_tokens: size,
    cumulative_cost_amount: "1.25",
    cumulative_cost_currency: "USD",
    sample_origin: "provider",
    observed_elapsed_ns: String(sequence * 10 - 1),
  });
}

interface UsageValues {
  readonly availability: "available" | "partial" | "unavailable";
  readonly source: "acp.prompt_response.usage" | null;
  readonly unavailable_reason:
    | "prompt_not_submitted"
    | "source_unsupported"
    | "usage_not_reported"
    | "turn_settlement_unknown"
    | null;
  readonly provider_total_tokens: string | null;
  readonly input_tokens: string | null;
  readonly output_tokens: string | null;
  readonly thought_tokens: string | null;
  readonly cached_read_tokens: string | null;
  readonly cached_write_tokens: string | null;
}

function actUsage(sequence: number, actId: string, values: UsageValues): DiagnosticEvent {
  return event({
    kind: "act_token_usage_finalized",
    schema_version: 1,
    run_id: RUN_ID,
    sequence: String(sequence),
    elapsed_ns: String(sequence * 10),
    scope: actScope(actId),
    caused_by: [],
    ...values,
  });
}

function stateFrom(events: readonly DiagnosticEvent[]) {
  return events.reduce(
    (state, diagnosticEvent) => reduceDiagnosticState(state, {
      type: "event_received",
      event: diagnosticEvent,
    }),
    createDiagnosticState(RUN_ID, decodeU64("0")),
  );
}

function aggregateField(
  knownSum: string | null,
  reported: string,
  finalized: string,
): UsageFieldAggregateSnapshot {
  return {
    known_sum: knownSum === null ? null : decodeTokenInteger(knownSum),
    reported_acts: decodeU64(reported),
    finalized_acts: decodeU64(finalized),
  };
}

function aggregate(
  scopeKind: SelectedUsageAggregate["scope_kind"],
  scopeLabel: string,
  finalized: string,
  reported: string,
  available: string,
  partial: string,
  unavailable: string,
  field: UsageFieldAggregateSnapshot,
): SelectedUsageAggregate {
  return {
    scope_kind: scopeKind,
    scope_label: scopeLabel,
    aggregate: {
      finalized_acts: decodeU64(finalized),
      reported_acts: decodeU64(reported),
      available_acts: decodeU64(available),
      partial_acts: decodeU64(partial),
      unavailable_acts: decodeU64(unavailable),
      provider_total_tokens: field,
      input_tokens: field,
      output_tokens: field,
      thought_tokens: field,
      cached_read_tokens: field,
      cached_write_tokens: field,
    },
  };
}

function stateFromUsageSnapshot() {
  const response = decodeSnapshotResponse(loadHttpFixture("snapshot-v1.json"));
  let state = createDiagnosticState(
    RUN_ID,
    response.watermark_sequence,
    response.state.through_elapsed_ns,
  );
  state = reduceDiagnosticState(state, {
    type: "usage_snapshot_received",
    snapshot: response.state.usage,
  });
  return reduceDiagnosticState(state, {
    type: "select",
    selection: hierarchyScopeReference(actScope("act-1"), "actor_id"),
  });
}

describe("usage diagnostics", () => {
  it("keeps live context occupancy separate from pending Act accounting across compaction", () => {
    let state = stateFrom([
      actStarted(1, "act-pending"),
      contextSample(2, "act-pending", "900", "1000"),
    ]);
    const view = render(<UsagePanel state={state} />);

    const context = screen.getByRole("region", { name: "Live context" });
    expect(within(context).getByRole("progressbar", { name: "Context occupancy" }))
      .toHaveAttribute("value", "90");
    expect(within(context).getAllByText("900")).toHaveLength(1);
    expect(within(context).getAllByText("1,000")).toHaveLength(1);
    expect(within(screen.getByTestId("act-accounting-act-pending")).getByText("Pending"))
      .toBeInTheDocument();

    state = reduceDiagnosticState(state, {
      type: "event_received",
      event: contextSample(3, "act-pending", "300", "1000"),
    });
    view.rerender(<UsagePanel state={state} />);

    expect(within(context).getByRole("progressbar", { name: "Context occupancy" }))
      .toHaveAttribute("value", "30");
    expect(within(context).queryByText("900")).not.toBeInTheDocument();
    expect(within(context).getAllByText("300")).toHaveLength(1);
    expect(screen.getByTestId("act-accounting-act-pending")).toHaveTextContent("Pending");
    expect(view.container.textContent).not.toMatch(/-\s*\d/);
  });

  it("distinguishes available, partial, and unavailable accounting without replacing unknowns with zero", () => {
    const huge = "1234567890123456789012345678901234567890";
    const state = stateFrom([
      actUsage(1, "act-available", {
        availability: "available",
        source: "acp.prompt_response.usage",
        unavailable_reason: null,
        provider_total_tokens: huge,
        input_tokens: "0",
        output_tokens: "77",
        thought_tokens: "12345678901234567890",
        cached_read_tokens: null,
        cached_write_tokens: null,
      }),
      actUsage(2, "act-partial", {
        availability: "partial",
        source: "acp.prompt_response.usage",
        unavailable_reason: null,
        provider_total_tokens: null,
        input_tokens: "5",
        output_tokens: null,
        thought_tokens: null,
        cached_read_tokens: null,
        cached_write_tokens: null,
      }),
      actUsage(3, "act-unavailable", {
        availability: "unavailable",
        source: null,
        unavailable_reason: "source_unsupported",
        provider_total_tokens: null,
        input_tokens: null,
        output_tokens: null,
        thought_tokens: null,
        cached_read_tokens: null,
        cached_write_tokens: null,
      }),
    ]);

    render(<UsagePanel state={state} />);

    const availableCard = screen.getByTestId("act-accounting-act-available");
    expect(within(availableCard).getByText("Available")).toBeInTheDocument();
    expect(within(availableCard).getByText("acp.prompt_response.usage"))
      .toBeInTheDocument();
    expect(within(availableCard).getByText("0")).toBeInTheDocument();
    expect(within(availableCard).getByText(
      "1,234,567,890,123,456,789,012,345,678,901,234,567,890",
    )).toBeInTheDocument();
    expect(within(availableCard).getByText("12,345,678,901,234,567,890"))
      .toBeInTheDocument();

    const partialCard = screen.getByTestId("act-accounting-act-partial");
    expect(within(partialCard).getByText("Partial")).toBeInTheDocument();
    expect(within(partialCard).getAllByText("Unknown")).toHaveLength(5);
    expect(within(partialCard).getByText("5")).toBeInTheDocument();

    const unavailableCard = screen.getByTestId("act-accounting-act-unavailable");
    expect(within(unavailableCard).getByText("Unavailable")).toBeInTheDocument();
    expect(within(unavailableCard).getByText("Source Unsupported")).toBeInTheDocument();
    expect(within(unavailableCard).getAllByText("Unknown")).toHaveLength(7);
    expect(unavailableCard.textContent).not.toContain("thought content");
  });

  it("renders Run, Scene, and Actor facts selected from the decoded server snapshot", () => {
    render(<UsagePanel state={stateFromUsageSnapshot()} />);

    const run = screen.getByTestId("usage-aggregate-run");
    const scene = screen.getByTestId("usage-aggregate-scene");
    const actor = screen.getByTestId("usage-aggregate-actor");
    expect(within(run).getByRole("heading", { name: "Run" })).toBeInTheDocument();
    expect(within(scene).getByRole("heading", { name: "scene-1" })).toBeInTheDocument();
    expect(within(actor).getByRole("heading", { name: "actor-1" })).toBeInTheDocument();
    expect(within(run).getByText(
      "1,234,567,890,123,456,789,012,345,678,901,234,567,890",
    )).toBeInTheDocument();
    expect(within(run).getAllByText("Complete known total")).toHaveLength(6);
    expect(screen.getByTestId("act-accounting-act-1")).toHaveTextContent("Available");
  });

  it("shows per-field aggregate coverage without completeness inflation", () => {
    const aggregates = [
      aggregate(
        "run",
        "Production run",
        "2",
        "2",
        "2",
        "0",
        "0",
        aggregateField("123456789012345678901234567890", "2", "2"),
      ),
      aggregate(
        "scene",
        "Scene scene-1",
        "3",
        "1",
        "0",
        "1",
        "2",
        aggregateField("99", "1", "3"),
      ),
      aggregate(
        "actor",
        "Actor actor-1",
        "1",
        "0",
        "0",
        "0",
        "1",
        aggregateField(null, "0", "1"),
      ),
    ] as const;

    render(<UsageCoverage aggregates={aggregates} />);

    const run = screen.getByTestId("usage-aggregate-run");
    const scene = screen.getByTestId("usage-aggregate-scene");
    const actor = screen.getByTestId("usage-aggregate-actor");
    expect(within(run).getByRole("heading", { name: "Production run" }))
      .toBeInTheDocument();
    expect(within(scene).getByRole("heading", { name: "Scene scene-1" }))
      .toBeInTheDocument();
    expect(within(actor).getByRole("heading", { name: "Actor actor-1" }))
      .toBeInTheDocument();
    expect(within(run).getAllByText("123,456,789,012,345,678,901,234,567,890"))
      .toHaveLength(6);
    expect(within(run).getAllByText("Complete known total")).toHaveLength(6);
    expect(within(scene).getAllByText("1 / 3 Acts")).toHaveLength(6);
    expect(within(scene).getAllByText("Known partial total")).toHaveLength(6);
    expect(within(scene).queryByText("Complete known total")).not.toBeInTheDocument();
    expect(within(scene).getByLabelText("Scene accounting availability"))
      .toHaveTextContent("Finalized3Reported1Available0Partial1Unavailable2");
    expect(within(actor).getAllByText("Unknown")).toHaveLength(6);
    expect(within(actor).queryByText("Complete known total")).not.toBeInTheDocument();
  });

  it("formats exact integers without numeric coercion and bounds only the context display ratio", () => {
    expect(formatExactInteger(decodeTokenInteger(
      "1234567890123456789012345678901234567890",
    ))).toBe("1,234,567,890,123,456,789,012,345,678,901,234,567,890");
    expect(formatExactInteger(decodeTokenInteger("0"))).toBe("0");
    expect(contextOccupancyPercent(decodeU64("3"), decodeU64("10"))).toBe(30);
    expect(contextOccupancyPercent(decodeU64("11"), decodeU64("10"))).toBe(100);
    expect(contextOccupancyPercent(decodeU64("0"), decodeU64("0"))).toBeNull();
    expect(contextOccupancyPercent(null, decodeU64("10"))).toBeNull();
  });

  it("consumes W08 projections and keeps aggregation and thought content out of the panel", () => {
    const panel = readFileSync(resolve(process.cwd(), "src/usage/UsagePanel.tsx"), "utf8");
    const coverage = readFileSync(resolve(process.cwd(), "src/usage/UsageCoverage.tsx"), "utf8");
    const css = readFileSync(resolve(process.cwd(), "src/usage/usage.css"), "utf8");

    expect(panel).toContain("presentedLiveEdge(state)");
    expect(panel).toContain("selectUsagePanelFacts(state)");
    expect(panel).toContain("edge.projection.context_usage.items");
    expect(panel).toContain("facts.usages");
    expect(panel).not.toContain("validatedAggregates");
    expect(panel).not.toContain("state.live.events");
    expect(panel).not.toContain("thought_content");
    expect(panel).not.toContain(".reduce(");
    expect(coverage).not.toContain(".reduce(");
    expect(coverage).not.toContain("BigInt(");
    expect(coverage).not.toMatch(/provider_total_tokens\s*[+\-=].*input_tokens/);
    expect(css).toContain("grid-template-columns: repeat(auto-fit, minmax(9rem, 1fr))");
    expect(css).toContain("@media (max-width: 42rem)");
    expect(css).toContain("overflow-wrap: anywhere");
    expect(css).not.toContain(".card");
    const radii = [...css.matchAll(/border-radius:\s*([0-9]+)px/g)]
      .map((match) => Number(match[1]));
    expect(radii.length).toBeGreaterThan(0);
    expect(radii.every((radius) => radius <= 8)).toBe(true);
  });
});
