import type { U64String } from "../protocol/decimal.ts";
import {
  SSE_CONTROL_NAMES,
  decodeSseFrame,
  type DecodedSseFrame,
  type SseControlName,
} from "../protocol/sse.ts";
import {
  DiagnosticTransportError,
  assertSameOriginUrl,
  diagnosticApiUrl,
  type DiagnosticBootstrap,
} from "./bootstrap.ts";


export interface EventSourceConnection {
  readonly readyState: number;
  addEventListener(type: string, listener: EventListenerOrEventListenerObject): void;
  removeEventListener(type: string, listener: EventListenerOrEventListenerObject): void;
  close(): void;
}

export interface EventSourceConstructor {
  new(url: string | URL): EventSourceConnection;
}

export interface DiagnosticEventStreamOptions {
  readonly bootstrap: Pick<DiagnosticBootstrap, "origin" | "api_base_url">;
  readonly after: U64String;
  readonly EventSource?: EventSourceConstructor;
  readonly onOpen: () => void;
  readonly onFrame: (frame: DecodedSseFrame) => void;
  readonly onError: () => void;
  readonly onProtocolError: (error: unknown) => void;
}

export interface DiagnosticEventStream {
  readonly url: string;
  readonly source: EventSourceConnection;
  close(): void;
}

export function diagnosticEventStreamUrl(
  bootstrap: Pick<DiagnosticBootstrap, "origin" | "api_base_url">,
  after: U64String,
): URL {
  const url = diagnosticApiUrl(bootstrap, "events");
  url.searchParams.set("after", after);
  assertSameOriginUrl(url, bootstrap.origin);
  return url;
}

export function openDiagnosticEventStream(
  options: DiagnosticEventStreamOptions,
): DiagnosticEventStream {
  const Constructor = options.EventSource ?? globalThis.EventSource;
  if (typeof Constructor !== "function") {
    throw new DiagnosticTransportError(
      "browser_capability",
      "native EventSource is unavailable",
    );
  }
  const url = diagnosticEventStreamUrl(options.bootstrap, options.after);
  const source = new Constructor(url.href);
  let closed = false;

  const onOpen: EventListener = () => {
    if (!closed) {
      options.onOpen();
    }
  };
  const onError: EventListener = () => {
    if (!closed) {
      options.onError();
    }
  };
  const frameListeners = new Map<string, EventListener>();
  const addFrameListener = (name: "diagnostic_event" | SseControlName): void => {
    const listener: EventListener = (rawEvent) => {
      if (closed) {
        return;
      }
      const message = rawEvent as MessageEvent<string>;
      let frame: DecodedSseFrame;
      try {
        frame = decodeSseFrame({
          event: name,
          id: name === "diagnostic_event" ? message.lastEventId : null,
          data: message.data,
        });
      } catch (error) {
        options.onProtocolError(error);
        return;
      }
      options.onFrame(frame);
    };
    frameListeners.set(name, listener);
    source.addEventListener(name, listener);
  };

  source.addEventListener("open", onOpen);
  source.addEventListener("error", onError);
  addFrameListener("diagnostic_event");
  for (const name of SSE_CONTROL_NAMES) {
    addFrameListener(name);
  }

  return {
    url: url.href,
    source,
    close(): void {
      if (closed) {
        return;
      }
      closed = true;
      source.removeEventListener("open", onOpen);
      source.removeEventListener("error", onError);
      for (const [name, listener] of frameListeners) {
        source.removeEventListener(name, listener);
      }
      source.close();
    },
  };
}
