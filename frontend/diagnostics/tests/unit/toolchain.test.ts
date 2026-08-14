import { signal } from "@preact/signals";
import { Activity } from "lucide-preact";
import { createElement } from "preact";
import { describe, expect, it } from "vitest";


describe("pinned diagnostics frontend toolchain", () => {
  it("loads the selected runtime libraries without compatibility shims", async () => {
    window.matchMedia = (query: string) => ({
      matches: false,
      media: query,
      onchange: null,
      addListener: () => undefined,
      removeListener: () => undefined,
      addEventListener: () => undefined,
      removeEventListener: () => undefined,
      dispatchEvent: () => false,
    });
    const { default: uPlot } = await import("uplot");
    const value = signal(1);
    value.value += 1;

    expect(value.value).toBe(2);
    expect(createElement("span", null).type).toBe("span");
    expect(typeof Activity).toBe("function");
    expect(typeof uPlot).toBe("function");
  });
});
