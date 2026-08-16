import { readFileSync } from "node:fs";
import { resolve } from "node:path";

import { cleanup, fireEvent, render, screen } from "@testing-library/preact";
import "@testing-library/jest-dom/vitest";
import type { JSX } from "preact";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import {
  ViewPanelErrorBoundary,
  type ViewPanelIdentity,
} from "../../src/views/error_boundary.tsx";


const IDENTITY: ViewPanelIdentity = {
  panel_id: "metric_view",
  query_identity: "run:42:view:metric_view:W:19",
  selection_identity: "scene-1/actor-2/cue-3",
};

let consoleError: ReturnType<typeof vi.spyOn>;

beforeEach(() => {
  consoleError = vi.spyOn(console, "error").mockImplementation(() => undefined);
});

afterEach(() => {
  cleanup();
  consoleError.mockRestore();
});

function ThrowingRenderer({ message }: { readonly message: string }): never {
  throw new Error(message);
}

describe("ViewSpec panel-local error boundary", () => {
  it("replaces only the throwing renderer and leaves a sibling operable", () => {
    const siblingAction = vi.fn();
    render(
      <main>
        <ViewPanelErrorBoundary
          identity={IDENTITY}
          runtime={{ status: "ready" }}
          compatibility={{ status: "compatible" }}
        >
          <ThrowingRenderer message="metric renderer failed" />
        </ViewPanelErrorBoundary>
        <section aria-label="Canonical timeline">
          <button type="button" onClick={siblingAction}>Select cue</button>
        </section>
      </main>,
    );

    expect(screen.getByRole("alert")).toHaveTextContent("metric renderer failed");
    expect(screen.getByLabelText("Canonical timeline")).toBeInTheDocument();
    fireEvent.click(screen.getByRole("button", { name: "Select cue" }));
    expect(siblingAction).toHaveBeenCalledOnce();
  });

  it("preserves query and selection identity in the typed local error", () => {
    const observed = vi.fn();
    const { container } = render(
      <ViewPanelErrorBoundary
        identity={IDENTITY}
        runtime={{ status: "ready" }}
        compatibility={{ status: "compatible" }}
        onError={observed}
      >
        <ThrowingRenderer message="bad layout" />
      </ViewPanelErrorBoundary>,
    );
    const panel = container.querySelector(".view-panel-boundary");
    expect(panel).toHaveAttribute("data-panel-id", IDENTITY.panel_id);
    expect(panel).toHaveAttribute("data-query-identity", IDENTITY.query_identity);
    expect(panel).toHaveAttribute("data-selection-identity", IDENTITY.selection_identity);
    expect(screen.getByRole("alert")).toHaveAttribute("data-error-code", "renderer_exception");
    expect(observed).toHaveBeenCalledWith(
      { code: "renderer_exception", message: "bad layout" },
      IDENTITY,
    );
  });

  it("retry reconstructs only that child", () => {
    let shouldThrow = true;
    let targetRenders = 0;
    let siblingRenders = 0;
    const onRetry = vi.fn(() => {
      shouldThrow = false;
    });
    function Target(): JSX.Element {
      targetRenders += 1;
      if (shouldThrow) {
        throw new Error("retry me");
      }
      return <p>Recovered renderer</p>;
    }
    function Sibling(): JSX.Element {
      siblingRenders += 1;
      return <p>Stable sibling</p>;
    }
    render(
      <main>
        <ViewPanelErrorBoundary
          identity={IDENTITY}
          runtime={{ status: "ready" }}
          compatibility={{ status: "compatible" }}
          onRetry={onRetry}
        >
          <Target />
        </ViewPanelErrorBoundary>
        <Sibling />
      </main>,
    );
    const siblingBefore = siblingRenders;
    fireEvent.click(screen.getByRole("button", { name: "Retry view renderer" }));
    expect(screen.getByText("Recovered renderer")).toBeInTheDocument();
    expect(onRetry).toHaveBeenCalledWith(IDENTITY);
    expect(targetRenders).toBeGreaterThan(1);
    expect(siblingRenders).toBe(siblingBefore);
  });

  it("keeps Runtime and compatibility state outside renderer failure replacement", () => {
    const { container } = render(
      <ViewPanelErrorBoundary
        identity={IDENTITY}
        runtime={{ status: "failed", code: "query_context_lost", message: "query worker exited" }}
        compatibility={{
          status: "incompatible",
          reason: "newer_view_schema",
          supported_view_schema_version: 1,
          record_view_schema_version: 3,
        }}
      >
        <ThrowingRenderer message="renderer also failed" />
      </ViewPanelErrorBoundary>,
    );
    expect(container.querySelector('[data-runtime-state="failed"]')).toHaveTextContent("query worker exited");
    expect(container.querySelector('[data-compatibility-state="incompatible"]')).toHaveTextContent("schema 3");
    expect(screen.getAllByRole("alert")).toHaveLength(2);
    expect(screen.getByText("renderer also failed")).toBeInTheDocument();
  });

  it("resets the local boundary when query identity changes", () => {
    const first = render(
      <ViewPanelErrorBoundary
        identity={IDENTITY}
        runtime={{ status: "ready" }}
        compatibility={{ status: "compatible" }}
      >
        <ThrowingRenderer message="old query failed" />
      </ViewPanelErrorBoundary>,
    );
    expect(screen.getByText("old query failed")).toBeInTheDocument();

    const nextIdentity = { ...IDENTITY, query_identity: "run:42:view:metric_view:W:20" };
    first.rerender(
      <ViewPanelErrorBoundary
        identity={nextIdentity}
        runtime={{ status: "ready" }}
        compatibility={{ status: "compatible" }}
      >
        <p>New query renderer</p>
      </ViewPanelErrorBoundary>,
    );
    expect(screen.getByText("New query renderer")).toBeInTheDocument();
    expect(first.container.querySelector(".view-panel-boundary")).toHaveAttribute(
      "data-query-identity",
      nextIdentity.query_identity,
    );
  });

  it("renders exception text as inert plain text", () => {
    const payload = "<img src=x onerror=globalThis.compromised=true>";
    const { container } = render(
      <ViewPanelErrorBoundary
        identity={IDENTITY}
        runtime={{ status: "ready" }}
        compatibility={{ status: "compatible" }}
      >
        <ThrowingRenderer message={payload} />
      </ViewPanelErrorBoundary>,
    );
    expect(screen.getByRole("alert")).toHaveTextContent(payload);
    expect(container.querySelector("img, script, a")).toBeNull();
  });

  it("contains no renderer, query, transport, or application-wide error logic", () => {
    const source = readFileSync(resolve(process.cwd(), "src/views/error_boundary.tsx"), "utf8");
    expect(source).not.toMatch(/TimelineViewResponse|MetricViewResponse|TableViewResponse|TimeSeriesViewResponse/);
    expect(source).not.toMatch(/fetch\s*\(|EventSource|XMLHttpRequest|WebSocket/);
    expect(source).not.toMatch(/window\.onerror|unhandledrejection|addEventListener/);
    expect(source).not.toMatch(/dangerouslySetInnerHTML|innerHTML|insertAdjacentHTML/);
  });
});
