import { readFileSync } from "node:fs";
import { dirname, resolve } from "node:path";

import {
  cleanup,
  fireEvent,
  render,
  screen,
  waitFor,
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
  REGISTERED_VIEW_RENDERERS,
  type DiagnosticsLiveController,
  type DiagnosticsViewQueryClient,
} from "../../src/app.tsx";
import { decodeCanonicalUuid, decodeU64 } from "../../src/protocol/decimal.ts";
import type {
  AgentMessageDeltaEvent,
  DiagnosticScope,
  SpanStartedEvent,
} from "../../src/protocol/event.ts";
import { VIEW_RENDERERS } from "../../src/protocol/view.ts";
import type { LiveDiagnosticsState } from "../../src/live/reconnect.ts";
import type { DiagnosticState } from "../../src/state/model.ts";
import {
  type DiagnosticStateAction,
  createDiagnosticState,
  reduceDiagnosticState,
} from "../../src/state/reducer.ts";


const RUN_ID = decodeCanonicalUuid("12345678-1234-4234-9234-123456789abc");

function scope(cueId: string | null = null): DiagnosticScope {
  return {
    scene_id: "scene-main",
    actor_id: "actor-researcher",
    cue_id: cueId,
    effect_id: null,
    act_id: cueId === null ? null : `act-${cueId}`,
    tool_call_id: null,
    session_generation: decodeU64("1"),
  };
}

function span(
  sequence: number,
  spanKind: SpanStartedEvent["span_kind"],
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
    span_kind: spanKind,
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
    text_delta: `Output for ${cueId}`,
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

function bootstrap(mode: "interactive" | "static") {
  return {
    document_url: "http://diagnostics.test/",
    origin: "http://diagnostics.test",
    api_base_url: "http://diagnostics.test/api/v1/",
    identity: { run_id: RUN_ID },
    status: { source: "active" },
    compatibility: {
      mode,
      decisions: {},
      missingBrowserCapabilities: mode === "static" ? ["EventSource"] : [],
    },
  } as unknown as NonNullable<LiveDiagnosticsState["bootstrap"]>;
}

function liveState(
  source: "active" | "archive" = "active",
  diagnostics: DiagnosticState | null = diagnosticState(),
): LiveDiagnosticsState {
  const interactive = bootstrap("interactive");
  return {
    phase: source === "archive" ? "archive" : "live",
    connection: source === "archive" ? "archive" : "connected",
    security: "trusted_network",
    security_scope: "trusted_network",
    outcome: source === "archive" ? "completed" : "running",
    bootstrap: {
      ...interactive,
      status: { ...interactive.status, source } as typeof interactive.status,
    },
    status: { source } as unknown as NonNullable<LiveDiagnosticsState["status"]>,
    snapshot: null,
    diagnostics,
    terminal_reason: null,
    error: null,
  };
}

class FakeLiveController implements DiagnosticsLiveController {
  state: LiveDiagnosticsState;
  readonly listeners = new Set<(state: LiveDiagnosticsState) => void>();
  readonly start = vi.fn(async (): Promise<void> => undefined);
  readonly stop = vi.fn((): void => undefined);

  constructor(state: LiveDiagnosticsState) {
    this.state = state;
  }

  subscribe(listener: (state: LiveDiagnosticsState) => void): () => void {
    this.listeners.add(listener);
    return () => this.listeners.delete(listener);
  }

  dispatch(action: DiagnosticStateAction): void {
    if (this.state.diagnostics === null) {
      throw new Error("diagnostics are absent");
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

function emptyViewClient(): DiagnosticsViewQueryClient {
  return {
    loadCatalog: vi.fn(async () => ({
      api_schema_version: 1,
      run_id: RUN_ID,
      capabilities: {} as never,
      views: [],
    })),
    query: vi.fn(),
    reportRendererFailure: vi.fn(),
    invalidateView: vi.fn(),
    dispose: vi.fn(),
  } as unknown as DiagnosticsViewQueryClient;
}

function localImports(path: string): readonly string[] {
  const source = readFileSync(path, "utf8").replace(
    /import\s+type\s+[\s\S]*?from\s+["'][^"']+["'];/g,
    "",
  );
  const imports: string[] = [];
  for (const match of source.matchAll(/from\s+["'](\.[^"']+)["']/g)) {
    const specifier = match[1];
    if (specifier !== undefined && !specifier.endsWith(".css")) {
      imports.push(resolve(dirname(path), specifier));
    }
  }
  return imports;
}

function importCycles(entry: string): readonly string[] {
  const cycles: string[] = [];
  const complete = new Set<string>();
  const visit = (path: string, stack: readonly string[]): void => {
    const position = stack.indexOf(path);
    if (position >= 0) {
      cycles.push([...stack.slice(position), path].join(" -> "));
      return;
    }
    if (complete.has(path)) {
      return;
    }
    for (const dependency of localImports(path)) {
      visit(dependency, [...stack, path]);
    }
    complete.add(path);
  };
  visit(entry, []);
  return cycles;
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

describe("diagnostics application composition", () => {
  it("registers every closed renderer exactly once and keeps the application import graph acyclic", () => {
    expect(REGISTERED_VIEW_RENDERERS).toEqual(VIEW_RENDERERS);
    expect(new Set(REGISTERED_VIEW_RENDERERS).size).toBe(VIEW_RENDERERS.length);
    expect(importCycles(resolve(process.cwd(), "src/app.tsx"))).toEqual([]);
    expect(readFileSync(resolve(process.cwd(), "src/app.tsx"), "utf8")).not.toMatch(
      /decodeDiagnostic|reduceDiagnosticState|openDiagnosticEventStream/,
    );
  });

  it("assembles all primary panels around one controller state and exposes an empty View surface", async () => {
    const controller = new FakeLiveController(liveState());
    const client = emptyViewClient();
    render(
      <App
        liveController={controller}
        viewClientFactory={() => client}
        productionName="research-production"
      />,
    );

    expect(screen.getByLabelText("Canonical production timeline")).toBeInTheDocument();
    expect(screen.getAllByRole("tab").map((tab) => tab.textContent)).toEqual([
      "Timeline",
      "Agent",
      "Events",
      "Usage",
      "Views",
    ]);

    fireEvent.click(screen.getByRole("tab", { name: "Agent" }));
    expect(screen.getByLabelText("Agent transcript")).toHaveTextContent("Output for cue-a");
    expect(screen.getByLabelText("Agent transcript")).toHaveTextContent("Output for cue-b");

    fireEvent.click(screen.getByRole("tab", { name: "Events" }));
    expect(screen.getByLabelText("Event explorer")).toBeInTheDocument();
    expect(screen.getByLabelText("Event inspector")).toBeInTheDocument();

    fireEvent.click(screen.getByRole("tab", { name: "Usage" }));
    expect(screen.getByRole("heading", { name: "Final Act accounting" })).toBeInTheDocument();

    fireEvent.click(screen.getByRole("tab", { name: "Views" }));
    await waitFor(() => expect(screen.getByLabelText("Compiled views")).toHaveAttribute(
      "data-state",
      "empty",
    ));
    expect(client.query).not.toHaveBeenCalled();
    expect(controller.start).toHaveBeenCalledOnce();
  });

  it("keeps archive panels reachable and makes global compatibility a static zero-query state", async () => {
    const archiveClient = emptyViewClient();
    const archive = render(
      <App
        liveController={new FakeLiveController(liveState("archive"))}
        viewClientFactory={() => archiveClient}
      />,
    );
    expect(screen.getByText("Archive")).toBeInTheDocument();
    fireEvent.click(screen.getByRole("tab", { name: "Views" }));
    await waitFor(() => expect(screen.getByLabelText("Compiled views")).toHaveAttribute(
      "data-state",
      "empty",
    ));
    archive.unmount();

    const staticState: LiveDiagnosticsState = {
      phase: "compatibility",
      connection: "offline",
      security: "trusted_network",
      security_scope: "trusted_network",
      outcome: "running",
      bootstrap: bootstrap("static"),
      status: null,
      snapshot: null,
      diagnostics: null,
      terminal_reason: null,
      error: null,
    };
    const factory = vi.fn(() => emptyViewClient());
    render(<App liveController={new FakeLiveController(staticState)} viewClientFactory={factory} />);
    expect(screen.getByLabelText("Compatibility status")).toBeInTheDocument();
    expect(factory).not.toHaveBeenCalled();
    expect(archiveClient.query).not.toHaveBeenCalled();
  });
});
