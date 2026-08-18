import { compareU64, type U64String } from "../protocol/decimal.ts";
import {
  decodeEventsResponse,
  type EventsResponse,
} from "../protocol/http.ts";
import {
  DiagnosticTransportError,
  diagnosticApiUrl,
  fetchSameOriginJson,
  type DiagnosticBootstrap,
  type DiagnosticFetch,
} from "../live/bootstrap.ts";


export interface TimelineHistoryCapture {
  readonly through: U64String;
  readonly response: EventsResponse;
}

export function diagnosticTimelineHistoryUrl(
  bootstrap: Pick<DiagnosticBootstrap, "origin" | "api_base_url">,
  through: U64String,
): URL {
  const url = diagnosticApiUrl(bootstrap, "events");
  url.searchParams.set("after", "0");
  url.searchParams.set("through", through);
  return url;
}

export async function fetchTimelineHistoryCapture(
  bootstrap: Pick<DiagnosticBootstrap, "origin" | "api_base_url" | "identity">,
  through: U64String,
  fetchImpl: DiagnosticFetch = globalThis.fetch,
): Promise<TimelineHistoryCapture> {
  if (typeof fetchImpl !== "function") {
    throw new DiagnosticTransportError("browser_capability", "native fetch is unavailable");
  }
  const response = decodeEventsResponse(await fetchSameOriginJson(
    diagnosticTimelineHistoryUrl(bootstrap, through),
    bootstrap.origin,
    fetchImpl,
    "diagnostic Timeline history",
  ));
  if (response.run_id !== bootstrap.identity.run_id) {
    throw new DiagnosticTransportError("identity", "Timeline history belongs to another Run");
  }
  if (compareU64(response.captured_watermark, through) < 0) {
    throw new DiagnosticTransportError(
      "protocol",
      "Timeline history was captured before the requested watermark",
    );
  }
  if (response.next_after !== null) {
    throw new DiagnosticTransportError(
      "protocol",
      "Timeline history unexpectedly returned a continuation cursor",
    );
  }

  let expected = 1n;
  const final = BigInt(through);
  for (const event of response.events) {
    if (BigInt(event.sequence) !== expected || BigInt(event.sequence) > final) {
      throw new DiagnosticTransportError(
        "protocol",
        `Timeline history is not the exact dense range (0,${through}]`,
      );
    }
    expected += 1n;
  }
  if (expected !== final + 1n) {
    throw new DiagnosticTransportError(
      "protocol",
      `Timeline history is not the exact dense range (0,${through}]`,
    );
  }
  return { through, response };
}
