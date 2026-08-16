import {
  cleanup,
  fireEvent,
  render,
  screen,
  waitFor,
  within,
} from "@testing-library/preact";
import "@testing-library/jest-dom/vitest";
import {
  afterEach,
  beforeEach,
  describe,
  expect,
  it,
  vi,
} from "vitest";

vi.mock("uplot", () => ({ default: class MockUPlot {} }));

import {
  App,
  type DiagnosticsLiveController,
  type DiagnosticsViewQueryClient,
} from "../../src/app.tsx";
import type { LiveDiagnosticsState } from "../../src/live/reconnect.ts";
import { decodeCanonicalUuid, decodeU64 } from "../../src/protocol/decimal.ts";
import type {
  AgentMessageDeltaEvent,
  DiagnosticScope,
  SpanStartedEvent,
} from "../../src/protocol/event.ts";
import {
  type ViewRecord,
  decodeViewRecord,
  decodeViewResponse,
} from "../../src/protocol/view.ts";
import {
  type ViewQueryContext,
  freezeViewQueryGeneration,
} from "../../src/query/binding.ts";
import {
  type ViewCatalog,
  type ViewQueryResult,
  ViewQueryLocalError,
  toTimeSeriesColumnarModel,
} from "../../src/query/client.ts";
import { freezeViewPagination } from "../../src/query/pagination.ts";
import type { DiagnosticState } from "../../src/state/model.ts";
import {
  type DiagnosticStateAction,
  createDiagnosticState,
  reduceDiagnosticState,
} from "../../src/state/reducer.ts";
import { scopeFromReference } from "../../src/state/selection.ts";
import { loadHttpFixture, loadViewFixture } from "../support/diagnostic-fixtures.ts";


const RUN_ID = decodeCanonicalUuid("12345678-1234-4234-9234-123456789abc");

interface ViewFixture {
  readonly descriptor: unknown;
  readonly response: unknown;
}

function scope(cueId: string | null = null): DiagnosticScope {
  return {
    scene_id: "scene-main",
    actor_id: "actor-shared",
    cue_id: cueId,
    effect_id: null,
    act_id: cueId === null ? null : `act-${cueId}`,
    tool_call_id: null,
    session_generation: decodeU64("1"),
  };
}

function span(
  sequence: number,
  kind: SpanStartedEvent["span_kind"],
  eventScope: DiagnosticScope,
): SpanStartedEvent {
  return {
    kind: "span_started",
    schema_version: 1,
    run_id: RUN_ID,
    sequence: decodeU64(String(sequence)),
    elapsed_ns: decodeU64(String(sequence * 10)),
    scope: eventScope,
    caused_by: [],
    span_kind: kind,
    detail: {},
    parent_span_id: null,
  };
}

function message(sequence: number, cueId: string): AgentMessageDeltaEvent {
  return {
    kind: "agent_message_delta",
    schema_version: 1,
    run_id: RUN_ID,
    sequence: decodeU64(String(sequence)),
    elapsed_ns: decodeU64(String(sequence * 10)),
    scope: scope(cueId),
    caused_by: [],
    message_id: `message-${cueId}`,
    source_message_id: null,
    text_delta: `Message for ${cueId}`,
  };
}

function diagnosticState(): DiagnosticState {
  return [
    span(1, "actor.handle_lifetime", scope()),
    span(2, "cue.execution", scope("cue-a")),
    message(3, "cue-a"),
    span(4, "cue.execution", scope("cue-b")),
    message(5, "cue-b"),
  ].reduce(
    (state, event) => reduceDiagnosticState(state, { type: "event_received", event }),
    createDiagnosticState(RUN_ID, decodeU64("0")),
  );
}

function liveState(): LiveDiagnosticsState {
  const bootstrap = {
    document_url: "http://diagnostics.test/",
    origin: "http://diagnostics.test",
    api_base_url: "http://diagnostics.test/api/v1/",
    identity: { run_id: RUN_ID },
    status: { source: "active" },
    compatibility: { mode: "interactive", decisions: {}, missingBrowserCapabilities: [] },
  } as unknown as NonNullable<LiveDiagnosticsState["bootstrap"]>;
  return {
    phase: "live",
    connection: "connected",
    security: "trusted_network",
    security_scope: "trusted_network",
    outcome: "running",
    bootstrap,
    status: bootstrap.status,
    snapshot: null,
    diagnostics: diagnosticState(),
    terminal_reason: null,
    error: null,
  };
}

class FakeLiveController implements DiagnosticsLiveController {
  state = liveState();
  readonly listeners = new Set<(state: LiveDiagnosticsState) => void>();

  async start(): Promise<void> {}
  stop(): void {}

  subscribe(listener: (state: LiveDiagnosticsState) => void): () => void {
    this.listeners.add(listener);
    return () => this.listeners.delete(listener);
  }

  dispatch(action: DiagnosticStateAction): void {
    if (this.state.diagnostics === null) {
      throw new Error("diagnostic state is unavailable");
    }
    this.state = {
      ...this.state,
      diagnostics: reduceDiagnosticState(this.state.diagnostics, action),
    };
    for (const listener of this.listeners) {
      listener(this.state);
    }
  }
}

function decodedFixture(file: string): { readonly record: ViewRecord; readonly response: ViewQueryResult["response"] } {
  const fixture = loadViewFixture(file) as ViewFixture;
  const record = decodeViewRecord(fixture.descriptor);
  const raw = structuredClone(fixture.response) as Record<string, unknown>;
  if (record.renderer === "time_series") {
    const series = raw.series as { readonly points: { value: unknown }[] }[];
    for (const item of series) {
      for (const point of item.points) {
        if (point.value !== null) {
          point.value = {
            aggregate: "exact",
            value: { type: "integer", value: "9007199254740992" },
          };
        }
      }
    }
  }
  return { record, response: decodeViewResponse(raw, record) };
}

function catalogAndResponses(): {
  readonly catalog: ViewCatalog;
  readonly responses: ReadonlyMap<string, ViewQueryResult["response"]>;
} {
  const metric = decodedFixture("metric.json");
  const table = decodedFixture("table.json");
  const timeline = decodedFixture("timeline.json");
  const timeSeries = decodedFixture("timeseries.json");
  const base = loadHttpFixture("view-catalog-v1.json") as {
    readonly capabilities: ViewCatalog["capabilities"];
  };
  const catalog: ViewCatalog = {
    api_schema_version: 1,
    run_id: RUN_ID,
    capabilities: base.capabilities,
    views: [
      metric.record,
      {
        status: "incompatible",
        view_id: "future_view",
        renderer: "metric",
        incompatible: {
          reason: "newer_view_schema",
          supported_view_schema_version: 1,
          record_view_schema_version: 2,
        },
      },
      table.record,
      timeline.record,
      timeSeries.record,
    ],
  };
  return {
    catalog,
    responses: new Map([
      [metric.record.id, metric.response],
      [table.record.id, table.response],
      [timeline.record.id, timeline.response],
      [timeSeries.record.id, timeSeries.response],
    ]),
  };
}

class FakeViewClient implements DiagnosticsViewQueryClient {
  readonly queryCalls: { readonly view_id: string; readonly context: ViewQueryContext }[] = [];
  readonly catalog: ViewCatalog;
  readonly responses: ReadonlyMap<string, ViewQueryResult["response"]>;
  readonly dispose = vi.fn((): void => undefined);
  readonly invalidateView = vi.fn((_viewId: string): void => undefined);

  constructor() {
    const fixture = catalogAndResponses();
    this.catalog = fixture.catalog;
    this.responses = fixture.responses;
  }

  async loadCatalog(): Promise<ViewCatalog> {
    return this.catalog;
  }

  async query(viewId: string, context: ViewQueryContext): Promise<ViewQueryResult> {
    this.queryCalls.push({ view_id: viewId, context });
    const record = this.catalog.views.find((entry): entry is ViewRecord => (
      !("status" in entry) && entry.id === viewId
    ));
    const response = this.responses.get(viewId);
    if (record === undefined || response === undefined) {
      throw new ViewQueryLocalError("view_not_found", `missing ${viewId}`);
    }
    return {
      generation: freezeViewQueryGeneration(RUN_ID, record, context),
      pagination: freezeViewPagination(record, this.catalog.capabilities),
      response,
      time_series: response.renderer === "time_series"
        ? toTimeSeriesColumnarModel(response)
        : null,
    };
  }

  reportRendererFailure(_viewId: string, failure: unknown): ViewQueryLocalError {
    return new ViewQueryLocalError(
      "renderer",
      failure instanceof Error ? failure.message : String(failure),
    );
  }
}

function lastQuery(client: FakeViewClient) {
  return client.queryCalls[client.queryCalls.length - 1];
}

beforeEach(() => {
  vi.stubGlobal("requestAnimationFrame", vi.fn(() => 1));
  vi.stubGlobal("cancelAnimationFrame", vi.fn());
});

afterEach(() => {
  cleanup();
  vi.unstubAllGlobals();
  window.history.replaceState(null, "", "#/timeline");
});

describe("application live and compiled-view assembly", () => {
  it("preserves manifest order, dispatches only compatible queries, and selects the matching renderer", async () => {
    const client = new FakeViewClient();
    render(
      <App
        liveController={new FakeLiveController()}
        viewClientFactory={() => client}
        productionName="multi-cue-production"
      />,
    );
    fireEvent.click(screen.getByRole("tab", { name: "Views" }));

    const catalog = await screen.findByRole("tablist", { name: "Compiled view catalog" });
    expect(within(catalog).getAllByRole("tab").map((tab) => tab.textContent)).toEqual([
      "Act input mean",
      "future_view",
      "Completed messages",
      "Cue timeline",
      "Queue depth",
    ]);
    await waitFor(() => expect(client.queryCalls.map((call) => call.view_id)).toEqual([
      "act_input_mean",
    ]));
    expect(screen.getByRole("heading", { name: "Act input mean" })).toBeInTheDocument();

    fireEvent.click(within(catalog).getByRole("tab", { name: "future_view" }));
    await waitFor(() => expect(screen.getByText(/newer than supported schema/)).toBeInTheDocument());
    expect(client.queryCalls).toHaveLength(1);

    fireEvent.click(within(catalog).getByRole("tab", { name: "Completed messages" }));
    await waitFor(() => expect(screen.getByRole("heading", { name: "Completed messages" })).toBeInTheDocument());
    expect(lastQuery(client)?.view_id).toBe("message_table");

    fireEvent.click(within(catalog).getByRole("tab", { name: "Cue timeline" }));
    await waitFor(() => expect(screen.getByRole("heading", { name: "Cue timeline" })).toBeInTheDocument());
    expect(lastQuery(client)?.view_id).toBe("cue_timeline");

    fireEvent.click(within(catalog).getByRole("tab", { name: "Queue depth" }));
    await waitFor(() => expect(screen.getByRole("heading", { name: "Queue depth" })).toBeInTheDocument());
    expect(document.querySelector(".timeseries-renderer")).toBeInTheDocument();
    expect(lastQuery(client)?.view_id).toBe("queue_depth");
  });

  it("keeps two cues of one Actor distinct across tree, timeline, transcript, and query binding", async () => {
    const controller = new FakeLiveController();
    const client = new FakeViewClient();
    render(
      <App
        liveController={controller}
        viewClientFactory={() => client}
        productionName="multi-cue-production"
      />,
    );
    fireEvent.click(screen.getByRole("tab", { name: "Views" }));
    await screen.findByRole("tablist", { name: "Compiled view catalog" });
    await waitFor(() => expect(client.queryCalls.length).toBeGreaterThan(0));

    fireEvent.click(screen.getByRole("button", { name: "Cue cue-a" }));
    await waitFor(() => expect(lastQuery(client)?.context.selected_scope?.cue_id).toBe("cue-a"));
    const cueASelection = controller.state.diagnostics?.presentation.selection ?? null;
    expect(cueASelection?.kind).toBe("scope");
    expect(cueASelection === null ? null : scopeFromReference(cueASelection)?.cue_id).toBe("cue-a");

    fireEvent.click(screen.getByRole("tab", { name: "Timeline" }));
    expect(document.querySelector(".timeline-treegrid__row[data-selected='true']")).toHaveTextContent(
      "Cue cue-a",
    );
    fireEvent.click(screen.getByRole("tab", { name: "Agent" }));
    expect(document.querySelector("[data-cue-id='cue-a']")).toHaveTextContent("Message for cue-a");
    expect(document.querySelector("[data-cue-id='cue-b']")).toHaveTextContent("Message for cue-b");

    fireEvent.click(screen.getByRole("button", { name: "Cue cue-b" }));
    fireEvent.click(screen.getByRole("tab", { name: "Views" }));
    await waitFor(() => expect(lastQuery(client)?.context.selected_scope?.cue_id).toBe("cue-b"));
    const cueBSelection = controller.state.diagnostics?.presentation.selection ?? null;
    expect(cueBSelection).not.toEqual(cueASelection);
    expect(cueBSelection === null ? null : scopeFromReference(cueBSelection)?.cue_id).toBe("cue-b");
  });
});
