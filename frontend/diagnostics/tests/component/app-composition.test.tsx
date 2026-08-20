import {
  cleanup,
  fireEvent,
  render,
  screen,
  waitFor,
} from "@testing-library/preact";
import "@testing-library/jest-dom/vitest";
import { afterEach, describe, expect, it, vi } from "vitest";

import {
  App,
  type DiagnosticsLiveController,
} from "../../src/app.tsx";
import type { LiveDiagnosticsState } from "../../src/live/reconnect.ts";
import { decodeCanonicalUuid, decodeU64 } from "../../src/protocol/decimal.ts";
import type { DiagnosticStateAction } from "../../src/state/reducer.ts";
import { createDiagnosticState } from "../../src/state/reducer.ts";

const RUN_ID = decodeCanonicalUuid("12345678-1234-4234-9234-123456789abc");

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
    status: { source: "active" } as never,
    snapshot: null,
    diagnostics: createDiagnosticState(RUN_ID, decodeU64("0")),
    terminal_reason: null,
    error: null,
  };
}

class FakeController implements DiagnosticsLiveController {
  state = liveState();
  readonly start = vi.fn(async (): Promise<void> => undefined);
  readonly stop = vi.fn((): void => undefined);
  readonly listeners = new Set<(state: LiveDiagnosticsState) => void>();

  subscribe(listener: (state: LiveDiagnosticsState) => void): () => void {
    this.listeners.add(listener);
    return () => this.listeners.delete(listener);
  }

  dispatch(_action: DiagnosticStateAction): void {
    for (const listener of this.listeners) listener(this.state);
  }

  publish(state: LiveDiagnosticsState): void {
    this.state = state;
    for (const listener of this.listeners) listener(state);
  }
}

afterEach(() => {
  cleanup();
  vi.unstubAllGlobals();
  vi.useRealTimers();
});

describe("diagnostics application composition", () => {
  it("mounts one Timeline surface and no secondary pages or extension panels", async () => {
    const controller = new FakeController();
    render(
      <App
        liveController={controller}
        productionName="timeline-only-production"
      />,
    );

    expect(screen.getByText("Troupe Timeline")).toBeInTheDocument();
    expect(screen.getByRole("button", { name: "Pause live diagnostics" })).toBeInTheDocument();
    expect(screen.getByRole("button", { name: "Follow live edge" })).toBeInTheDocument();
    expect(screen.getByRole("button", { name: "History" })).toBeInTheDocument();
    expect(screen.queryByText("Agent")).not.toBeInTheDocument();
    expect(screen.queryByText("Events")).not.toBeInTheDocument();
    expect(screen.queryByText("Usage")).not.toBeInTheDocument();
    expect(screen.queryByText("Views")).not.toBeInTheDocument();
    expect(screen.queryByText(/Available lane/i)).not.toBeInTheDocument();
    await waitFor(() => expect(controller.start).toHaveBeenCalledOnce());
  });

  it("falls back to a static compatibility surface without creating a panel client", () => {
    const controller = new FakeController();
    controller.state = {
      ...controller.state,
      phase: "compatibility",
      connection: "offline",
      outcome: "running",
      diagnostics: null,
    };
    render(<App liveController={controller} />);
    expect(screen.getByLabelText("Compatibility status")).toBeInTheDocument();
  });

  it("coalesces bursty controller notifications into one latest presentation update", async () => {
    vi.useFakeTimers();
    const controller = new FakeController();
    render(<App liveController={controller} />);
    await vi.advanceTimersByTimeAsync(0);

    for (let sequence = 1; sequence <= 100; sequence += 1) {
      controller.publish({
        ...controller.state,
        diagnostics: createDiagnosticState(
          RUN_ID,
          decodeU64(String(sequence)),
          decodeU64(String(sequence * 1_000_000_000)),
        ),
      });
    }

    expect(screen.getByTitle("Run running")).toHaveTextContent("0");
    await vi.advanceTimersByTimeAsync(249);
    expect(screen.getByTitle("Run running")).toHaveTextContent("0");
    await vi.advanceTimersByTimeAsync(1);
    expect(screen.getByTitle("Run running")).toHaveTextContent("100");
  });

  it("loads an exact frozen capture before enabling History playback", async () => {
    const controller = new FakeController();
    const historyFetch = vi.fn(async (_input: Parameters<typeof fetch>[0]): Promise<Response> => new Response(JSON.stringify({
      api_schema_version: 1,
      run_id: RUN_ID,
      captured_watermark: "0",
      events: [],
      next_after: null,
    }), { status: 200, headers: { "content-type": "application/json" } }));
    render(<App liveController={controller} historyFetch={historyFetch} />);

    fireEvent.click(screen.getByRole("button", { name: "History" }));
    expect(screen.getByRole("button", { name: "Play History range" })).toBeDisabled();
    await waitFor(() => {
      expect(screen.getByRole("button", { name: "Play History range" })).toBeEnabled();
    });
    expect(historyFetch).toHaveBeenCalledOnce();
    expect(String(historyFetch.mock.calls[0]?.[0])).toBe(
      "http://diagnostics.test/api/v1/events?after=0&through=0",
    );
  });
});
