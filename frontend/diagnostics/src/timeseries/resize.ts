export interface TimeSeriesPlotSize {
  readonly width: number;
  readonly height: number;
}

const MINIMUM_WIDTH = 160;
const MINIMUM_HEIGHT = 220;
const DEFAULT_HEIGHT = 280;

export function normalizeTimeSeriesPlotSize(
  width: number,
  height = DEFAULT_HEIGHT,
): TimeSeriesPlotSize {
  const finiteWidth = Number.isFinite(width) ? Math.floor(width) : MINIMUM_WIDTH;
  const finiteHeight = Number.isFinite(height) ? Math.floor(height) : DEFAULT_HEIGHT;
  return {
    width: Math.max(MINIMUM_WIDTH, finiteWidth),
    height: Math.max(MINIMUM_HEIGHT, finiteHeight),
  };
}

export class TimeSeriesResizeController {
  readonly #element: HTMLElement;
  readonly #onResize: (size: TimeSeriesPlotSize) => void;
  readonly #height: number;
  #observer: ResizeObserver | null = null;
  #frame: number | null = null;
  #pending: TimeSeriesPlotSize | null = null;
  #closed = false;

  constructor(
    element: HTMLElement,
    onResize: (size: TimeSeriesPlotSize) => void,
    height = DEFAULT_HEIGHT,
  ) {
    this.#element = element;
    this.#onResize = onResize;
    this.#height = height;
    this.#schedule(element.getBoundingClientRect().width);
    if (typeof ResizeObserver !== "undefined") {
      this.#observer = new ResizeObserver((entries) => {
        const entry = entries.find((item) => item.target === this.#element);
        if (entry !== undefined) {
          this.#schedule(entry.contentRect.width);
        }
      });
      this.#observer.observe(element);
    }
  }

  #schedule(width: number): void {
    if (this.#closed) {
      return;
    }
    this.#pending = normalizeTimeSeriesPlotSize(width, this.#height);
    if (this.#frame !== null) {
      return;
    }
    if (typeof requestAnimationFrame === "undefined") {
      this.#flush();
      return;
    }
    this.#frame = requestAnimationFrame(() => {
      this.#frame = null;
      this.#flush();
    });
  }

  #flush(): void {
    const size = this.#pending;
    this.#pending = null;
    if (!this.#closed && size !== null) {
      this.#onResize(size);
    }
  }

  disconnect(): void {
    if (this.#closed) {
      return;
    }
    this.#closed = true;
    this.#observer?.disconnect();
    this.#observer = null;
    this.#pending = null;
    if (this.#frame !== null && typeof cancelAnimationFrame !== "undefined") {
      cancelAnimationFrame(this.#frame);
    }
    this.#frame = null;
  }
}
